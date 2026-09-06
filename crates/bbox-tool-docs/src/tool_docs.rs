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
    Workflows,
    Whiteboards,
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
            Self::Workflows => "Workflow orchestration",
            Self::Whiteboards => "Whiteboards",
            Self::Roadmap => "Roadmap",
            Self::StorageHealth => "Storage health",
            Self::Workspace => "Workspace tools",
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
                "Structured side channel for *notable* observations surfaced during delegated work — orchestrators query `bbox_notes` / `bbox_inbox` at round boundaries. Seven kinds: `dispute`, `assumption`, `surprise`, `followup`, `blocked`, `learned`, `done`. Emit one only when you have something genuinely worth flagging; this is a signal channel, not a progress log, and silence is the right default when nothing is notable. A `done` note with a one-line acceptance summary is useful when a caller (atom, workflow, or an explicit completion contract) asks for a structured sign-off — it is not required on every dispatch."
            }
            Self::Gaps => {
                "First-class substrate gap-note store. File a gap when the blocker is in the blackbox substrate or shared agent workflow — a missing tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, or runbook that agents in other projects could plausibly hit too — not in the current product codebase. Project-scoped gaps are repo-owned (committed under `<project>/.bbox/gaps/`, travel with the checkout); cross-project substrate gaps go to the central host store with `scope=\"global\"`. `bbox_gap` files (typed, validated, deduped by `dedupe_key`), `bbox_gaps` filters by typed fields, `bbox_gap_resolve` closes out (with structured supersession), `bbox_gap_update` edits in place. See `sm-gap-notes` via `bbox_knowledge` for the full envelope, vocabularies, and lifecycle."
            }
            Self::Inbox => {
                "Attention aggregator: a single read that surfaces unresolved notes, stale threads, unverified knowledge, and failed tasks. Run at round boundaries, morning-brief style, and whenever you're unsure what needs attention next."
            }
            Self::Artifacts => {
                "Versioned install catalog for producer-side workflows, rule-packets, and brofiles. Use this surface instead of hand-copying shipped artifacts into daemon state; metadata tracks source, active version, and supersession."
            }
            Self::Packets => {
                "Reusable judges compiled from examples or stated rules. If your task involves writing a priority-ordered rubric, ranking a batch against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones — compile a packet. `bbox_compile` authors the mechanism, `bbox_apply` evaluates any entity deterministically (no LLM), `bbox_audit` self-validates against known labels. Packets are portable: dispatch `packet_id` to sub-agents and every one of them produces bit-identical output. See `sm-rule-packets` via `bbox_knowledge` for the full runbook."
            }
            Self::Orchestration => {
                "Dispatch agents across providers (Claude, GLM, DeepSeek, Inception, Codex, Copilot, Vibe, Gemini). Prefer named `bro` targeting (resolves provider + account + lens + context + session automatically) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again — it starts fresh with no memory). For ensembles: `bro_broadcast` + `bro_when_all` (blind deliberation) or `bro_when_any` (race). For provider-default suppression and minimal probe/team context, pull `sm-brofile-context` via `bbox_knowledge`."
            }
            Self::Workflows => {
                "Define multi-phase agent protocols as JSON specs with per-node `next` transitions and dispatch them as a unit. The daemon owns the state machine; actors (executor / ensemble) are dispatched INTO the loop as stateless turns — persona / role / contract is the brofile lens, not an engine type. `atom_bindings` let nodes invoke standalone atom contracts directly. Gate packets route choice nodes by verdict; retry ceilings cap back-edges; fork + `late_inject` express async steering; sub-workflows compose arcs like rule-packets compose via `Apply`; workflow-level `policy_packet` mechanizes arc-health decisions without an LLM advisor. Whiteboards (see `whiteboard_*` tools) provide multi-agent deliberation with phases + structured posts; `wait_for_phase` resumes arcs on board transitions. Every run opens a workflow-origin `bbox_thread(kind=work_item)` with structured notes + rolling compaction anchors; normal `bbox_thread_list` calls hide workflow-origin threads unless `include_workflows=true`. Replaces long skill-prose protocols (overmind, crucible). See `sm-workflow-orchestration` via `bbox_knowledge` for the full runbook and `examples/workflows/` for the catalog."
            }
            Self::Whiteboards => {
                "Multi-agent deliberation surface. Posts (proposals / claims / concerns / informational), annotations (challenge / corroborate / resolve / validation), and votes accumulate on a board, advanced through phases (blind → read → validate → debate → resolve → archived) by a facilitator-or-operator role. Three audiences share one surface: in-workflow ensemble specialists (their structured outputs auto-post when the node has a `board:` field), in-workflow facilitators (single bro, drives transitions), and external agents — operator's Claude session, dispatched help, eventually humans through slack / ntfy adapters — that read state via `whiteboard_state` and act via `whiteboard_post` / `whiteboard_vote` / `whiteboard_transition`. Phase transitions emit `board-transitioned` signals through the same `dispatch_routed_event` pipeline webhooks use; arcs `wait_for_phase` to resume when the board advances. Replaces phaser as a peer external MCP server."
            }
            Self::Roadmap => {
                "Operator-directed prospective work tracker: designed-but-not-implemented features, refactors, explorations, tech debt, and risks. Roadmap interactions are performed only at the express direction of the operator; never use the roadmap to defer, postpone, or avoid requested implementation work. Inbox is reactive; threads are active work; knowledge is atemporal. Use `action=\"next\"` to rank accepted items, and `action=\"promote\"` to spin a roadmap item into a work thread."
            }
            Self::StorageHealth => "Read-only storage inventory for edge sidecar hygiene.",
            Self::Workspace => {
                "Instrumented file read, shell execution, and git operations for registered projects. Prefer these over raw Read/Bash/git when working inside a bbox-registered project — every call is indexed as a tool-call record and enriched with bbox context where relevant."
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
        ToolCategory::Workflows => Some("sm-workflow-orchestration"),
        ToolCategory::Whiteboards => Some("sm-whiteboards"),
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
    ToolCategory::Workflows,
    ToolCategory::Whiteboards,
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
        when_to_use: "Use when you know the topic but not the exact session. Filter by account, project, or role early. Pass `exclude_self=true` for current-turn searches. `source` filters the lane a document came from (`glm`, `claude`, `codex`, `gemini`, `slack`, ...): comma-separated for several, and a `-` prefix excludes one, so `source=\"slack\"` searches only ingested Slack conversations and `source=\"-slack\"` searches everything else. Slack conversations are searchable by default; that one filter is how you include or exclude them. For \"what's in a channel\" questions reach for `channel=` first: it accepts a channel name (leading `#` accepted) or channel id, resolves names through the current roster to the stable channel id so a renamed channel still matches its whole history, and also matches documents stamped with the queried name. Plain queries match channel names too, so a bare `query=\"ops-incident-4565\"` surfaces that channel's messages even when no message body names it. Authorship on a conversation hit is identity, not turn kind, so filter who spoke with `author=<provider user id>`; the `role` lane only distinguishes human from app there. Conversation hits render the channel, the author, and a derived Slack permalink; their `file_path` (a `slack:<workspace>/<channel>` locator) and `session_id` (a per-channel-per-day bucket) both drill down directly through `bbox_context` / `bbox_messages`, resolved against the conversation landing store rather than a transcript file — `channel=` and the permalink remain the other two working paths. See `sm-transcript-retrieval` for ladders.",
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
        summary: "Return cheap route embedding health and health_reason. include_coverage explicitly requests a full source-corpus coverage scan; include_diagnostics requests deadline-bounded HNSW graph diagnostics; recall_probe_route runs a sampled self-recall probe (all opt-ins can be expensive).",
        when_to_use: "Use when vector search degrades. The default reports availability, queue depth, success count, capped_count (enqueues rejected at the queue cap - residue the sweeper will refill, not a drop), dropped_count (permanently un-embeddable poison), and sanitized error without walking the source corpus or HNSW. Pass include_coverage=true for exact per-route source/indexed counts and stalled-coverage classification; this walks every embedding-source document and can take minutes on a large corpus. Pass include_diagnostics=true with an optional bounded diagnostic_routes list for deadline-bounded connectivity diagnostics; unavailable is reported separately from healthy. Pass recall_probe_route for sampled self-recall; explicit probes can take seconds on large partitions and refuse busy routes.",
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
        summary: "Corpus statistics (doc count, index size, file counts).",
        when_to_use: "Sanity-check the index; diagnose 'did my new sessions get indexed?'.",
        example: None,
    },
    // ── Agentic graph ────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_inspect_entity",
        category: ToolCategory::Graph,
        summary: "Inspect properties and targeted edges. Filter edge_types and direction; per_type_limit=0 reads properties only. property_mode selects summary, smart, or full. Follow edge_page.next_cursor for more edges; property retrieves exact text in pages.",
        when_to_use: "Use after search to verify a ref and inspect relevant relations. Select edge_types and direction (out, in, both). property_mode is summary, smart (default, 300-character text previews), or full; invalid values fail. Edges page at 100 maximum; follow edge_page.next_cursor as edge_cursor with the same selection. Read a property key from properties or property_projection.omitted_keys with property=<key>; body.next_cursor continues via property_cursor. property_limit is 4..4096 UTF-8 bytes, default 4096. Cursors reject changed selections or source revisions. Schema-authored absent relations remain explicit; generic empty scaffolding is omitted. Evidence properties retain assertion authority, source generation, endpoint freshness, and unresolved states. No embedded rendered text mirror is returned.",
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
        summary: "Orient to entity types, edge families, and traversal. include_agents=true or mode=\"full\" adds installed agents and consultants.",
        when_to_use: "Use once for graph vocabulary and traversal orientation. Default omits agent and consultant catalogs; include_agents=true or mode=full adds them. mode=agents is a deprecated full alias; unknown modes fail. dispatch_adapter remains on each agent row. No rendered text mirror or duplicate agent grouping is returned.",
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
        summary: "Read-only accepted-publication status and runtime health for one catalog project. Reports current, prior-fallback, missing, or corrupt state; accepted scope, ref, commit, generation identity, pointer SHA-256, and the typed source binding. Attachment bindings name an attachment; producer bindings name the producer plus source generation id and evidence hash. It also reports scope agreement and advance availability. A `health` object adds the bounded runtime projection: binding status, source kind, recorded attachment capabilities, overlay outcomes, and watcher state. An `auto_advance` object reports the standing operator grant (enabled, the audit reason that installed it, and whether the accepted binding is producer-bound and therefore eligible) plus the last policy attempt and why it did or did not move the pointer. Generation id and pointer SHA-256 are the compare-and-swap tokens bbox_project_publisher_advance requires. The response also echoes the project's catalog `scope`, and for a connector source adds a `connector` object naming the connector_source_id, the connector kind, the producer's observed vendor coordinates, the publication_lanes it claims, and a `file_source` object carrying that lane's state: the active generation with its ordinal, producer, collected selector, document and file counts, logical bytes, cursor epoch, manifest digest, and the producer's publication telemetry (entries enumerated, blobs fetched, documents exported, per-reason skip counters); the per-state generation counts; and the last reported cursor degradation with its cause and cost. `remote_watermark_display_only` is named for what it is and is never freshness authority. A connector project that has onboarded without publishing reports an absent active generation rather than an error, and an unreadable lane reports readable=false with a diagnostic instead of failing the whole call. No credential material appears anywhere in this response. The call is observational, takes no checkout lease, and is path-free. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use before an advance to read compare-and-swap tokens, or to diagnose unavailable, prior-fallback, or non-advancing published knowledge. The typed source binding distinguishes local attachment authority from a retained remote producer generation, and the auto_advance object is where a Ready candidate that is not serving explains itself.",
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
        when_to_use: "",
        example: None,
    },
    ToolDoc {
        name: "bbox_forget",
        category: ToolCategory::Knowledge,
        summary: "Retire or supersede an entry.",
        when_to_use: "Entry is stale or replaced. Prefer `bbox_decide` with `supersedes` if the replacement is itself a decision.",
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
        when_to_use: "Use to approve or reject existing unverified entries. Review controls render eligibility; it does not import rendered-file edits. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
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
        when_to_use: "Before starting work on a topic (continuity check). Use `status` for lifecycle (`open`, `active`, `resolved`, `promoted`) and `min_idle_days` to return only threads idle for at least N days. Filter by `kind=work_item`. Workflow-origin arc threads are hidden by default; pass `include_workflows=true` when you intentionally want workflow scaffolding.",
        example: None,
    },
    // ── Notes ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_note",
        category: ToolCategory::Notes,
        summary: "Record a structured side-channel note while working.",
        when_to_use: "Emit a note only when you have a *notable* signal worth surfacing to an orchestrator — a `dispute`, `surprise`, `blocked`, `learned` fact, or actionable `followup`. Silence is the correct default: this is a side channel, not a per-call progress log, so most dispatches should emit nothing. A `kind=done` sign-off is opt-in — emit it when a caller (atom, workflow, team broadcast, or an explicit completion contract) asks for one, not on every dispatch. Use `learned` for agent-discovered facts, not user-stated rules. See `sm-side-channel-notes` via `bbox_knowledge` for the full note taxonomy. Substrate gaps that other projects could plausibly hit too are NOT side-channel notes — file them with `bbox_gap` (see `sm-gap-notes` via `bbox_knowledge`), not here.",
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
        when_to_use: "The mandatory dedupe step before `bbox_gap`: search open gaps by `dedupe_key`, `gap_kind`, `domain`, `impact`, or free-text `query` before filing. Also the triage surface — pass `json=true` for machine-readable records to group/extract, or `include_addressed=true` to see closed gaps. Addressed gaps are hidden by default for lists, shown by default for an exact `id`. `project` accepts a project_id, a registered operator alias, or a project path, and matches rows by project identity; a value that resolves to no registered project keeps literal substring matching and says so in `diagnostics`, so an empty list is never silent about an unresolvable filter.",
        example: Some(r#"bbox_gaps(gap_kind="mcp_surface", include_addressed=false)"#),
    },
    ToolDoc {
        name: "bbox_gap_resolve",
        category: ToolCategory::Gaps,
        summary: "Resolve a gap note (acknowledged/addressed); optionally wire a structured supersession link.",
        when_to_use: "Close-out move when a gap is implemented, rejected, superseded, or intentionally closed. `addressed` hides it from default views; `acknowledged` keeps it visible as deferred. Pass `superseded_by=gap-<id>` to retire a stale gap in favor of a better-shaped successor — it sets the structured supersedes/superseded_by link on both records. `project` (session cwd / worktree path; auto-filled on dispatches) is WRITE-TARGETING only and never required to FIND the gap: ids are globally unique, so an id-only call resolves the owning project from the same view `bbox_gaps` lists. From a recognized worktree the rewritten repo-owned gap file lands in that worktree so the session's branch carries the resolution — the gap's durable project scope never changes, and global gaps ignore it. The commit that fills a gap should also carry an `Addresses-Gap-Note: gap-<id>` trailer.",
        example: Some(
            r#"bbox_gap_resolve(id="gap-a1b2c3d4", resolution="addressed", note="implemented in commit abc123")"#,
        ),
    },
    ToolDoc {
        name: "bbox_gap_update",
        category: ToolCategory::Gaps,
        summary: "Edit an existing gap note's fields in place.",
        when_to_use: "Amend a gap with additional context/evidence discovered after filing — refine the title, wanted_capability, impact, blocking_level, missing_primitive, fallback_used, evidence, or notes — without creating a disjoint successor or re-filing. `project` (session cwd / worktree path; auto-filled on dispatches) is WRITE-TARGETING only and never required to find the gap by id: from a recognized worktree the rewritten repo-owned gap file lands in that worktree; the gap's durable project scope never changes, and global gaps ignore it.",
        example: Some(
            r#"bbox_gap_update(id="gap-a1b2c3d4", impact="high", evidence=["src/foo.rs:120", "thread-7f01324e"])"#,
        ),
    },
    // ── Inbox ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_inbox",
        category: ToolCategory::Inbox,
        summary: "Aggregate attention layer across every store.",
        when_to_use: "Round boundaries, morning brief, any 'what needs my attention' moment. Surfaces unresolved disputes/blocked/surprises, deferred followups, stale threads, unverified knowledge, failed bro tasks. Single call, prioritized view. Open gaps from the `bbox_gap` store surface here too; pass `import_gap_spool=true`, `aggregate_gaps=true`, or `check_gap_closeouts=true` for the gap workflow helpers. See `sm-gap-notes` via `bbox_knowledge`.",
        example: Some(r#"bbox_inbox(project="/repo/x", stale_days=3)"#),
    },
    // ── Artifact catalog ─────────────────────────────────────────────
    ToolDoc {
        name: "bbox_artifact_install",
        category: ToolCategory::Artifacts,
        summary: "Install a typed artifact from an inline artifact object or explicit HTTP(S) source URL. Supply exactly one; caller filesystem paths are rejected. The selected kind controls validation. Returns activation state and actionable warnings without source credentials or storage paths.",
        when_to_use: "Use for producer-side artifacts shipped under system-defaults/agentic-corpus, system-defaults/atoms, system-defaults/maintenance, or project-local .bbox directories. The installer validates and activates the artifact through the existing workflow, packet, brofile, agent, atom, team, or cron path, then records version/source/supersession metadata in the catalog. Team artifacts are teamplate-shaped and materialize on install: the teamplate store is written and the team instantiated under the teamplate's name (member brofiles must already be installed; re-install never clobbers a live team's sessions; advisor-carrying teamplates are rejected — use bro_team create).",
        example: Some(
            r#"bbox_artifact_install(kind="workflow", source="system-defaults/agentic-corpus/workflows/schema-migration-arc.json")"#,
        ),
    },
    ToolDoc {
        name: "bbox_artifact_list",
        category: ToolCategory::Artifacts,
        summary: "List installed artifact summary pages (default 20, maximum 100). Continue with next_offset; filter by kind/name and set detail=true for installation and supersession metadata. Storage paths and source credentials are omitted.",
        when_to_use: "Inventory check before installing or superseding producer machinery. Use kind/name filters to inspect a specific artifact family.",
        example: Some(r#"bbox_artifact_list(kind="packet")"#),
    },
    ToolDoc {
        name: "bbox_artifact_supersede",
        category: ToolCategory::Artifacts,
        summary: "Mark one installed artifact superseded by another artifact of the same kind.",
        when_to_use: "Use when a customized workflow/packet/brofile/agent replaces an installed version but you want the old version retained for audit.",
        example: Some(
            r#"bbox_artifact_supersede(kind="workflow", name="auto-digest-arc", superseded_by="auto-digest-arc-v2")"#,
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
        summary: "Discover compiled rule-packet summary pages (default 20, maximum 100), newest first then id. Filter by domain, scope, or query before paging; continue with next_offset. Select packet_id and detail=true for classification histograms and rule previews. latest_per_domain=true keeps the newest revision of each domain.",
        when_to_use: "Run BEFORE `bbox_compile` on any new domain. Query by concept (\"breaking\", \"pii\", \"deny\") when you don't know the exact domain label. If a match exists: reuse via `bbox_apply` or compose via `Apply{packet_id, expect}` inside your new packet — don't re-derive. Pair with `bbox_packet_events(packet_id=...)` to check the packet's track record (fidelity, no_match rate) before depending on it.",
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
        summary: "Launch a fresh agent task/session and return {taskId, sessionId}. Required selector: provide either `bro`, `provider`, or runtime allocation fields such as `tier`, `pool_name`, `pin_provider`, `pin_model`, or `capabilities`.",
        when_to_use: "Use to start a fresh agent session only. A dispatch selector is required: pass exactly one selector family — `bro` for a named bro, `provider` for a raw ad-hoc provider, or allocator fields (`tier`, `tier_ladder`, `tier_mode`, `min_tier`, `max_tier`, `pool_name`, `pool_providers`, `capabilities`, `selection_policy`, `pin_provider`, `pin_account`, `pin_model`, `pin_effort`, `prefer_provider`) for pool-backed runtime allocation. Set the session's working directory with `cwd` (canonical name; `project_dir` is accepted as a deprecated alias). Fresh-session overrides such as `service_tier` apply after selector resolution. Prefer `bro:` over raw `provider:` so routing stays stable when a named bro exists. Record `taskId`, `sessionId`, and any `selectionTraceId`; inspect allocation decisions with `bro_allocator_trace`. Without an account pin, allocation uses only that provider's declared default or native credentials; unrelated global accounts are not candidates. For any follow-up on that same work, use `bro_resume`; another `bro_exec` starts fresh and has no continuity.",
        example: Some(
            r#"bro_exec(prompt="review this patch", cwd="/repo/x", tier="standard", pool_name="coding", durable=true)"#,
        ),
    },
    ToolDoc {
        name: "bro_resume",
        category: ToolCategory::Orchestration,
        summary: "Continue an existing session with a follow-up; single-flight per provider session and the continuity path after bro_exec.",
        when_to_use: "Use for follow-ups on an existing bro session. Do not use `bro_exec` again when you need continuity. Pass explicit `session_id` / `provider` when possible; named bro targeting is only safe when the session is unambiguous. The working-directory override is `cwd` (canonical; `project_dir` accepted as a deprecated alias) — usually unnecessary because resume auto-resolves the session's recorded cwd. For Brodex, pass `service_tier=\"priority\"` to force fast routing for the continuation or `service_tier=\"default\"` to persist standard routing. `pin_model` / `pin_effort` override the model and reasoning effort for the resumed turns (absent ⇒ brofile/session default). Never call `bro_resume` on a session while its previous task is still running: first `bro_wait(task_id=...)`, or `bro_cancel(task_id=...)` if you are abandoning that turn. If a prior turn failed but the session is still useful, resume it with recovery context before starting a fresh `bro_exec`. See `sm-bro-dispatch-patterns` via `bbox_knowledge` for workflow shapes.",
        example: Some(
            r#"bro_resume(bro="executor", prompt="add tests for the edge case we discussed")"#,
        ),
    },
    ToolDoc {
        name: "bro_allocator_status",
        category: ToolCategory::Orchestration,
        summary: "Read pool-backed runtime allocation config, active leases, in-flight lane counts, and optional candidate preview.",
        when_to_use: "Use when debugging or auditing late-bound bro dispatch: inspect effective tier mappings, pools, selection policies, active runtime leases, probe state, and current in-flight lane counts. Pass tier/pool/capability/pin fields to preview the candidate table without spawning a task or writing a lease.",
        example: Some(
            r#"bro_allocator_status(project_dir="/repo/x", tier="standard", pool_name="coding")"#,
        ),
    },
    ToolDoc {
        name: "bro_allocator_trace",
        category: ToolCategory::Orchestration,
        summary: "Read a previous runtime allocation selection trace by id.",
        when_to_use: "Use when bro_exec returned selectionTraceId and you need to explain why the allocator selected or rejected provider/account/model lanes.",
        example: Some(r#"bro_allocator_trace(selection_trace_id="alloc-0123abcd")"#),
    },
    ToolDoc {
        name: "bro_allocator_probe",
        category: ToolCategory::Orchestration,
        summary: "Read, update, or clear allocator probe state for a provider/account lane.",
        when_to_use: "Use to record credential, quota, cooldown, and probe-confidence observations consumed by allocator scoring and bro_allocator_status previews. This mutates allocator/probes.json; use bro_allocator_status for read-only inspection.",
        example: Some(
            r#"bro_allocator_probe(provider="codex", quota_status="exhausted", quota_confidence="runtime_rate_limit", cooldown_ms=300000)"#,
        ),
    },
    ToolDoc {
        name: "badgey_exec",
        category: ToolCategory::Orchestration,
        summary: "Start a Badgey consultant instance for a project scope and return its badgey_id, provider session, task, and thread-of-record ids.",
        when_to_use: "Use when you want Badgey to consult over a project with continuity. The wrapper opens a work-item thread, dispatches the badgey brofile, and owns the session mapping. Use `badgey_resume` or `badgey_ask` for follow-up turns.",
        example: Some(
            r#"badgey_exec(project_dir="/repo/x", brief="help me navigate the agent graph work")"#,
        ),
    },
    ToolDoc {
        name: "badgey_resume",
        category: ToolCategory::Orchestration,
        summary: "Send a turn to an existing Badgey instance. Mechanical commands such as `dismiss` are handled by the wrapper before provider resume.",
        when_to_use: "Use for any follow-up where Badgey should keep its thread-of-record context. Calls are serialized per badgey_id so concurrent callers do not corrupt the provider session.",
        example: Some(
            r#"badgey_resume(badgey_id="bg-0123abcd-4567ef89", prompt="teach me why this edge matters")"#,
        ),
    },
    ToolDoc {
        name: "badgey_ask",
        category: ToolCategory::Orchestration,
        summary: "Question-shaped alias for badgey_resume.",
        when_to_use: "Use when the caller is asking a direct question of an existing Badgey instance and you prefer `question` over `prompt` in the request shape.",
        example: Some(
            r#"badgey_ask(badgey_id="bg-0123abcd-4567ef89", question="what should I inspect next?")"#,
        ),
    },
    ToolDoc {
        name: "badgey_dismiss",
        category: ToolCategory::Orchestration,
        summary: "Dismiss a Badgey instance, drain queued turns, write a dismiss event, and resolve its thread of record.",
        when_to_use: "Use when a Badgey consultation is done or should stop accepting turns. After dismissal, new resumes for that badgey_id fail with instance_dismissed.",
        example: Some(
            r#"badgey_dismiss(badgey_id="bg-0123abcd-4567ef89", reason="work complete")"#,
        ),
    },
    ToolDoc {
        name: "badgey_status",
        category: ToolCategory::Orchestration,
        summary: "Inspect one Badgey instance, including queue status and proposals; without badgey_id, returns active instances.",
        when_to_use: "Use to debug a Badgey consultation, see queue depth, inspect provider/session/thread bindings, or check proposal state before applying.",
        example: Some(r#"badgey_status(badgey_id="bg-0123abcd-4567ef89")"#),
    },
    ToolDoc {
        name: "badgey_list",
        category: ToolCategory::Orchestration,
        summary: "List Badgey instances and their thread/session bindings.",
        when_to_use: "Use when you need to find active Badgey instances or include dismissed records for audit.",
        example: Some(r#"badgey_list(include_dismissed=true)"#),
    },
    ToolDoc {
        name: "badgey_scout",
        category: ToolCategory::Orchestration,
        summary: "Ask Badgey to author scout sub-charters for a focused question; wrapper post-processing dispatches emitted scout actions.",
        when_to_use: "Use for bounded fan-out investigation when Badgey should decompose one question into focused scout turns without exposing bro_exec to the Badgey provider session.",
        example: Some(
            r#"badgey_scout(badgey_id="bg-0123abcd-4567ef89", charter="compare these two graph paths")"#,
        ),
    },
    ToolDoc {
        name: "badgey_collect",
        category: ToolCategory::Orchestration,
        summary: "Collect scout/sub-bro events for a Badgey instance or scout id.",
        when_to_use: "Use after badgey_scout or bg-action-spawn-subbro processing to see whether scout work is still walking or has produced dispatch records.",
        example: Some(r#"badgey_collect(badgey_id="bg-0123abcd-4567ef89")"#),
    },
    ToolDoc {
        name: "badgey_triage_inbox",
        category: ToolCategory::Orchestration,
        summary: "Produce a Badgey-shaped inbox triage proposal sheet for stale/open work in a scope.",
        when_to_use: "Use for morning-brief triage. The result is a proposal-sheet shape; applying concrete actions still goes through Badgey's proposal gate.",
        example: Some(r#"badgey_triage_inbox(scope="/repo/x")"#),
    },
    ToolDoc {
        name: "badgey_close_loops",
        category: ToolCategory::Orchestration,
        summary: "Classify dispatched tasks without done notes; never synthesizes executor done notes.",
        when_to_use: "Use for completion-contract audits. Results may identify suspected completions, crashes, or stalls, but the tool does not write kind=done on behalf of executors.",
        example: Some(r#"badgey_close_loops(window_days=14, project_dir="/repo/x")"#),
    },
    ToolDoc {
        name: "bro_wait",
        category: ToolCategory::Orchestration,
        summary: "Block until one task completes; timeout returns a snapshot, not proof the task is dead. If the result is empty or suspicious, inspect bro_status(tail=N) before resuming, cancelling, or treating it as success.",
        when_to_use: "After `bro_exec` or `bro_resume` when you need the result. USE MAXIMUM TIMEOUT for provider work. On timeout, or when a completed result is empty/suspicious, call `bro_status(tail=N)` before deciding the task is stuck, treating it as success, cancelling it, or dispatching replacement work. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value. Internal workflow consumers retain exact full exits.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_all",
        category: ToolCategory::Orchestration,
        summary: "Block until ALL tasks / team members complete; use for fan-out/fan-in instead of hand-rolled sequential waits.",
        when_to_use: "Fan-out/fan-in pattern. Pair with `bro_broadcast` for blind deliberation / provider comparison. USE MAXIMUM TIMEOUT. On timeout, inspect member status before cancelling or redispatching. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value. Internal workflow consumers retain exact full exits.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_any",
        category: ToolCategory::Orchestration,
        summary: "Block until the FIRST task completes; use for races instead of polling each task yourself.",
        when_to_use: "Racing providers / fast-path resolution. First result wins, others keep running unless cancelled. Before cancelling laggards, check status and cancel only if the remaining work is truly no longer useful. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value. Internal workflow consumers retain exact full exits.",
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
        summary: "Read task progress. detail=result, report, or structured_exit returns exact body pages; replay body.next_cursor to continue. debug adds execution diagnostics.",
        when_to_use: "Default summary includes state, progress, blockers and result availability. Result, report and structured_exit detail returns body.text with format, offset and total_bytes; pages contain at most 4096 UTF-8 bytes. Replay body.next_cursor unchanged with the same task and detail. A changed body rejects the cursor; restart from its first page. Reassemble JSON report/structured_exit pages before parsing. tail applies only to summary, capped at 50 events and 8192 serialized bytes. debug adds accounting and worker-owned transcript coordinates; those coordinates are not caller file paths. Context occupancy is telemetry, never remaining work capacity.",
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
        when_to_use: "Agents and workflow hooks call this at major milestones so bro_dashboard and bro_status show what the task last reported, what it needs, and when it last checked in.",
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
        when_to_use: "You want a finished bro to reflect on friction with the blackbox substrate itself — missing/awkward bbox_/bro_/work_ tools, stale guidance or memories, clumsy workflow/dispatch steps — and self-file substrate gaps via bbox_gap (surfaced in bbox_inbox) only if something's worth surfacing. Scoped to surfaces blackbox can change, not the target repo or its toolchain. Does not delete the task; bro_prune(retro=true) is the bulk path at cleanup time.",
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
        summary: "Manage teamplates and instantiated teams.",
        when_to_use: "Save templates, instantiate teams, inspect roster, or tear teams down. Team members resume existing sessions on later broadcasts; dissolve/recreate when validating new brofile context or provider-default suppression. See `sm-brofile-context` via `bbox_knowledge`. Before `save_template` or `create`, list existing objects first to avoid duplicates. Dissolve ad hoc teams you created after their work is terminal; do not dissolve another operator's team unless instructed. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
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
    ToolDoc {
        name: "bro_slack_bind",
        category: ToolCategory::Orchestration,
        summary: "Bind a Slack channel to a bbox project. The binding scopes inbound Slack→badgey activity to a single project and gives the daily-triage cron a per-channel home for proposal posts. Channel id (C-prefix) is the stable lookup key; rename-safe. Actions: bind, unbind, list, lookup. Project accepts absolute path or 8-hex project_id from the registry.",
        when_to_use: "Bind every project channel you want a daily brief in. Without bindings the triage cron is a no-op (deliberately no global fallback) and inbound app_mention / reaction events can't auto-resolve their project. Before `action=bind`, register the project via `bbox_project_register` so the binding captures a stable project_id.",
        example: Some(
            r#"bro_slack_bind(action="bind", team_id="T0123ABCD", channel_id="C0123XYZ", channel_name="transcript-search", project="/home/me/repos/transcript-search")"#,
        ),
    },
    ToolDoc {
        name: "badgey_proposals_list",
        category: ToolCategory::Orchestration,
        summary: "List Badgey proposal summaries by numeric id (default 20, maximum 100). Continue with next_after as after and the returned through bound, keeping since/only_pending unchanged. No drafts or history in list pages. proposal_id reads one exact draft; include_events=true adds transition history. Exact reads cannot combine list filters/cursors. Returns proposals[], count, has_more, next_after, through.",
        when_to_use: "Workflow node that needs the full proposal record (draft fields, state, etc.) — `badgey_resume` only returns proposal_id list, not the bodies. Pair with `since` set to the synthesis-turn start timestamp to scope to just-emitted proposals.",
        example: Some(
            r#"badgey_proposals_list(badgey_id="bg-deadbeef-cafef00d", since="2026-05-07T08:00:00Z", only_pending=true)"#,
        ),
    },
    ToolDoc {
        name: "badgey_ensure_for_channel",
        category: ToolCategory::Orchestration,
        summary: "Get-or-create the system Badgey instance that authors triage briefs for a Slack-bound project. Reads the (team_id, channel_id) binding to resolve the project scope, looks up the binding's badgey_id; if absent or the instance has been dismissed, exec a fresh Badgey instance, persist its id back on the binding, and return it. Used by the per-channel triage workflow's EnsureInstance node.",
        when_to_use: "Called from the per-channel triage workflow's first node. Requires a binding via `bro_slack_bind action=bind`. Idempotent — re-calling against an active instance returns the existing id with `created=false`.",
        example: Some(r#"badgey_ensure_for_channel(team_id="T0123ABCD", channel_id="C0123XYZ")"#),
    },
    ToolDoc {
        name: "bro_slack_link_lookup",
        category: ToolCategory::Orchestration,
        summary: "Resolve a Slack message ts back to its SlackProposalLink (proposal_id, instance_id, project_dir, version, posted_at). Used by the apply/refine workflows that fire on `:white_check_mark:` reactions and in-thread replies — they need the (BadgeyId, proposal_id) pair from the link to call badgey_apply_proposal or bro_resume. Returns {found: false} for messages that aren't a posted proposal (e.g. random check on an unrelated message) so workflows can no-op cleanly.",
        when_to_use: "First node of badgey-apply-proposal-arc and badgey-clarify-arc — branch on `found` to either continue (proposal exists) or terminate cleanly (random reaction/reply on a non-proposal message).",
        example: Some(
            r#"bro_slack_link_lookup(team_id="T0123ABCD", channel_id="C0123XYZ", msg_ts="1778179224.543499")"#,
        ),
    },
    ToolDoc {
        name: "badgey_apply_proposal",
        category: ToolCategory::Orchestration,
        summary: "Apply a stored BadgeyProposal — drives the wrapper's full apply path: state-machine transition (Pending/Failed → Applying), kind-specific dispatch (artifact_promotion → bbox_artifact_install; redispatch_task → spawn_privileged_task with the proposal's prompt; workflow_install/agent_install/packet_install → matching artifact install), record applied_task_id, transition (Applying → Applied | Failed). Returns the apply result with status. One-shot wrapper — for the Slack-reaction flow prefer the split `badgey_proposal_begin_apply` + `badgey_proposal_complete_apply` pair so the workflow engine tracks the dispatched bro natively as an actor node.",
        when_to_use: "Workflow / direct callers that want a one-shot apply. The Slack-reaction badgey-apply-proposal-arc uses the split pair instead so the dispatched bro is tracked as an actor node (visible in actor_results.<NodeId> for downstream PostOutcome rendering). Pass `retry_failed=true` only when explicitly retrying a proposal in Failed state.",
        example: Some(
            r#"badgey_apply_proposal(badgey_id="bg-deadbeef-cafef00d", proposal_id="P-3")"#,
        ),
    },
    ToolDoc {
        name: "badgey_proposal_begin_apply",
        category: ToolCategory::Orchestration,
        summary: "Phase 1 of the split apply path. Transitions a proposal Pending|Failed → Applying and returns dispatch parameters (prompt + brofile + label for redispatch_task; artifact_kind + source + version for artifact installs). Does NOT spawn the bro or install the artifact — the workflow caller does that via an actor node or `bbox_artifact_install` mcp_call, then calls `badgey_proposal_complete_apply` with the outcome. Lets the engine track the dispatched work natively (actor task lifecycle, retries, gates) instead of opaquely spawning behind a wrapper.",
        when_to_use: "First mcp_call inside badgey-apply-proposal-arc after the Slack link is resolved. Read the returned `outcome`: `redispatch` → run an actor with `prompt`; `install` → mcp_call bbox_artifact_install with the returned source/kind; `already_applied` → skip dispatch and skip the complete call (PostOutcome emits green directly); `rejected` → skip with a failure post.",
        example: Some(
            r#"badgey_proposal_begin_apply(badgey_id="bg-deadbeef-cafef00d", proposal_id="P-3")"#,
        ),
    },
    ToolDoc {
        name: "badgey_proposal_complete_apply",
        category: ToolCategory::Orchestration,
        summary: "Phase 2 of the split apply path. Given the outcome of the dispatched work (passed in `outcome`: `completed` / `failed` / `cancelled` / `timed_out`), transitions the proposal Applying → Applied or Applying → Failed and writes the audit decision. Always returns `{status: applied|failed, ...}` so the workflow's PostOutcome node can read the final state and pick the badge.",
        when_to_use: "Last mcp_call before PostOutcome in badgey-apply-proposal-arc. For the redispatch path, pass `outcome=${actor_results.Dispatch.status}`, `task_id=${actor_results.Dispatch.taskId}`, `summary=${actor_results.Dispatch.result}`. For the artifact-install path, pass `outcome=completed` on a successful install, with `artifact_ref=${vars.install_response.<artifact_ref>}`. Skip this call entirely on the `already_applied` / `rejected` short-circuit paths.",
        example: Some(
            r#"badgey_proposal_complete_apply(badgey_id="bg-deadbeef-cafef00d", proposal_id="P-3", outcome="completed", task_id="3c2df23e-...", summary="Done — fix landed at src/main.rs:669")"#,
        ),
    },
    ToolDoc {
        name: "consultant_proposals_list",
        category: ToolCategory::Orchestration,
        summary: "List consultant proposal summaries by numeric id (default 20, maximum 100). Continue with next_after as after and the returned through bound, keeping since/only_pending unchanged. No drafts or history in list pages. proposal_id reads one exact draft; include_events=true adds transition history. Exact reads cannot combine list filters/cursors. Returns proposals[], count, has_more, next_after, through.",
        when_to_use: "Workflow nodes that need full proposal records without hard-coding a consumer's tool name — pass `consumer` (e.g. `badgey`) plus the instance id. Prefer this over `badgey_proposals_list` in new consumer-agnostic arcs; the badgey_* form remains as the pinned shim.",
        example: Some(
            r#"consultant_proposals_list(consumer="badgey", consultant_id="bg-deadbeef-cafef00d", since="2026-05-07T08:00:00Z", only_pending=true)"#,
        ),
    },
    ToolDoc {
        name: "consultant_apply_proposal",
        category: ToolCategory::Orchestration,
        summary: "Apply a stored consultant proposal for any registered consumer — state-machine transition (Pending/Failed → Applying), kind-specific dispatch (artifact kinds → bbox_artifact_install; redispatch_task → privileged task spawn), record applied_task_id, transition (Applying → Applied | Failed). One-shot wrapper; workflow callers that want the engine to track the dispatched work natively should use the split `consultant_proposal_begin_apply` + `consultant_proposal_complete_apply` pair. Consumer-agnostic equivalent of `badgey_apply_proposal`.",
        when_to_use: "One-shot applies from consumer-agnostic callers. Returns `{status: applied|already_applied|failed|bad_input, summary}` like the badgey shim. Pass `retry_failed=true` only when explicitly retrying a Failed proposal.",
        example: Some(
            r#"consultant_apply_proposal(consumer="badgey", consultant_id="bg-deadbeef-cafef00d", proposal_id="P-3")"#,
        ),
    },
    ToolDoc {
        name: "consultant_proposal_begin_apply",
        category: ToolCategory::Orchestration,
        summary: "Phase 1 of the consumer-agnostic split apply path. Transitions a proposal Pending|Failed → Applying and returns dispatch parameters (prompt + brofile + label for redispatch_task; artifact_kind + source + version for artifact installs). Does NOT spawn the bro or install the artifact — the workflow caller does that via an actor node or `bbox_artifact_install` mcp_call, then calls `consultant_proposal_complete_apply` with the outcome. Consumer-agnostic equivalent of `badgey_proposal_begin_apply`.",
        when_to_use: "First mcp_call of a consumer-agnostic apply arc. Read the returned `outcome`: `redispatch` → run an actor with `prompt`; `install` → mcp_call bbox_artifact_install with the returned source/kind; `already_applied` → skip dispatch and the complete call; `rejected` → skip with a failure post.",
        example: Some(
            r#"consultant_proposal_begin_apply(consumer="badgey", consultant_id="bg-deadbeef-cafef00d", proposal_id="P-3")"#,
        ),
    },
    ToolDoc {
        name: "consultant_proposal_complete_apply",
        category: ToolCategory::Orchestration,
        summary: "Phase 2 of the consumer-agnostic split apply path. Given the outcome of the dispatched work (`completed` / `failed` / `cancelled` / `timed_out`), transitions the proposal Applying → Applied or Applying → Failed and writes the audit decision. Always returns `{status: applied|failed, ...}` so the workflow's outcome node can read the final state. Consumer-agnostic equivalent of `badgey_proposal_complete_apply`.",
        when_to_use: "Last mcp_call before the outcome node in a consumer-agnostic apply arc. For the redispatch path pass `outcome=${actor_results.Dispatch.status}` and `task_id=${actor_results.Dispatch.taskId}`; for the artifact-install path pass `outcome=completed` with the installed `artifact_ref`. Skip on the `already_applied` / `rejected` short-circuit paths.",
        example: Some(
            r#"consultant_proposal_complete_apply(consumer="badgey", consultant_id="bg-deadbeef-cafef00d", proposal_id="P-3", outcome="completed", task_id="3c2df23e-...", summary="Done")"#,
        ),
    },
    ToolDoc {
        name: "bro_slack_link_record",
        category: ToolCategory::Orchestration,
        summary: "Record a SlackProposalLink mapping a posted Slack message back to its BadgeyProposal. Called by the per-channel triage workflow's emit-proposal subworkflow after chat.postMessage so inbound reactions/replies can resolve back to (BadgeyId, proposal_id) and the apply/refine hooks fire.",
        when_to_use: "Workflow node hook after a successful chat.postMessage. Pass the msg_ts from the Slack response, the BadgeyProposal id, the BadgeyInstance id (so the apply hook resolves it), and the project_dir scope.",
        example: Some(
            r#"bro_slack_link_record(team_id="T0123ABCD", channel_id="C0123XYZ", msg_ts="1778179224.543499", proposal_id="P-3", instance_id="bg-deadbeef-cafef00d", project_dir="/repo/x")"#,
        ),
    },
    // ── Workflows ────────────────────────────────────────────────────
    ToolDoc {
        name: "bro_orchestrate_author",
        category: ToolCategory::Workflows,
        summary: "Compile a prose charter into a validated workflow spec. Dispatches an authoring LLM with the sm-workflow-orchestration runbook + a minimal reference example, parses its JSON response, cross-validates via the engine's compile step, retries once on compile failure with the error appended, and returns the validated spec — ready to pass to `bro_orchestrate_run`. Closes the authoring loop: operators describe the arc in prose, get a JSON spec back (with per-node `next` transitions), dispatch without hand-writing the graph.",
        when_to_use: "Use when you want a workflow but don't want to hand-write the JSON — describe the arc shape in prose (charter), pass an authoring brofile (e.g. `probe-haiku` or a Sonnet/Opus profile for richer outputs), optionally hint at a known pattern (`crucible`, `blind-convergence`, `optimistic-review`, `linear`), and the compiler returns a validated spec. Callers with an established house grammar can pass `exemplars` (few-shot workflow JSON specs, 64KB combined budget) and a `preamble` (domain ground truth injected ahead of the charter) so the compiler emits specs in that grammar instead of generic shapes. Gate/policy packet IDs come back as `packet-TODO` placeholders you fill in after compilation. Pair with `bro_orchestrate_run` for a prose-to-execution loop.",
        example: Some(
            r#"bro_orchestrate_author(charter="Review a proposal against 3 design criteria in parallel, aggregate findings, and route 'pass' or 'revise' to a final node", brofile="probe-haiku", hint="crucible")"#,
        ),
    },
    ToolDoc {
        name: "bro_orchestrate_run",
        category: ToolCategory::Workflows,
        summary: "Dispatch a workflow as a pollable task. Takes a full spec (actors, nodes with per-node `next` transitions: goto / branch / fork / terminal) and returns {taskId, arcId, status} immediately by default; poll with bro_status(task_id=...), await with bro_wait(task_id=...), or inspect arc state with bro_arc_status(arc_id=...). Pass await_completion=true only when the caller intentionally wants blocking behavior. Pass dry_run=true to validate + summarize without dispatching any bros. Run and dry-run both validate `subworkflow_ref` seams strictly: every ref must be installed and its imports/exports must type-check against the child schema. Workflows declaring `admission` enforce at most one non-terminal arc per key; a duplicate start errors with the holding arc named.",
        when_to_use: "Use when your task has multiple phases with verdict-based branching, retry-on-fail semantics, async steering (fork + late_inject), fanout (`foreach` / `matrix`), or reusable sub-arcs — and especially when you'd otherwise be writing dozens of lines of 'advisor MUST NOT … / protocol REQUIRES …' prose to keep a top-level LLM from drifting as it coordinates. Author the spec (or copy one from `examples/workflows/`), cross-validate via `dry_run=true`, then dispatch. Follow with `bro_status`, `bro_wait`, `bro_arc_status`, or `bbox_notes(thread_id=<arc_thread_id>)` for progress/audit. Full runbook at `sm-workflow-orchestration` via `bbox_knowledge`.",
        example: Some(
            r#"bro_orchestrate_run(workflow={...full spec...}, project_dir="/repo/x", dry_run=true)"#,
        ),
    },
    ToolDoc {
        name: "bro_arc_signal",
        category: ToolCategory::Workflows,
        summary: "Resolve a pending Wait by signal name + correlation tuple. Same dispatch path that the webhook router uses for `signal_arc` verdicts — surfaced as MCP so an operator can manually advance an arc that's blocked on an external event.",
        when_to_use: "Use to manually push an arc that's parked on a Wait node when the upstream event hasn't (or won't) arrive — e.g. testing, debugging, or rescuing an arc that missed its webhook. Empty `correlate` broadcasts to all matching waits.",
        example: Some(r#"bro_arc_signal(signal="pr-merged", correlate={"pr": 42})"#),
    },
    ToolDoc {
        name: "bro_arc_status",
        category: ToolCategory::Workflows,
        summary: "Inspect active/recent workflow positions and typed wait correlations. Select arc_id (arcId or arc_thread_id), or list deterministic summaries with limit (default 10, max 20), offset and next_offset. Summary omits completed-node history and visit-count maps and bounds waits. detail=full returns exact selected snapshots and waits as JSON body pages; continue cursor=body.next_cursor. Unknown arc ids and invalid detail/selectors are errors.",
        when_to_use: "Use to debug stuck arcs without parsing event logs — answers 'where is this arc and what's it waiting on?' in one shot. With no arc_id, lists every running arc plus all pending waits.",
        example: Some(r#"bro_arc_status(arc_id="thread-abc12345")"#),
    },
    ToolDoc {
        name: "bro_arc_result",
        category: ToolCategory::Workflows,
        summary: "Read a task-backed workflow result: structuredExit, selected vars, arcThreadId and actorSessions; include_node_outputs=true adds node prose. Accepts arcId or workflow task id. keys selects vars; default vars omit duplicate _structured_exit. Small selected results stay inline; large selections explicitly return a preview. detail=full returns exact selected JSON body pages; continue cursor=body.next_cursor with the same selectors and parse concatenated body.text. Webhook/SSE-ingress arcs are not task-backed.",
        when_to_use: "Use after a workflow finishes to consume its output — the replacement for parsing bro_wait's escaped result envelope. `keys` narrows to the vars you actually need (e.g. keys=[\"sieve\"]). Running arcs return {status: running}; pair with bro_arc_status for live position and bbox_notes(thread_id=<arc_thread_id>) for the audit trail.",
        example: Some(
            r#"bro_arc_result(arc_id="arc-c60058fe9116dad8465043a39987a76c", keys=["sieve"])"#,
        ),
    },
    ToolDoc {
        name: "bro_webhook_replay",
        category: ToolCategory::Workflows,
        summary: "Replay an arbitrary payload through an installed webhook's extractor + routing packet WITHOUT dispatching the verdict. Returns the extracted entity, the routing verdict's classification, and the resolved consequent (after `${entity.X}` substitution). Skips signature verification — same path as the HTTP `/webhook/:name/replay` endpoint, surfaced as MCP so routing-rule iteration happens inside the tool surface. Records the replay into the same delivery ring buffer (`source: replay`) so `bro_webhook_deliveries` shows it.",
        when_to_use: "Use to iterate on a routing-packet rule against a synthetic payload without needing the upstream code-host to fire a real event. Hand-craft (or copy from `bro_webhook_deliveries`) the body + headers that match what the upstream sends, replay, inspect the verdict, edit the rule, recompile the packet, replay again. Pairs with `bro_webhook_deliveries` for the captured-then-replay debug loop.",
        example: Some(
            r#"bro_webhook_replay(name="forgejo", body={"action":"synchronized","pull_request":{"number":42},"repository":{"name":"r","owner":{"login":"o"}}}, headers={"x-gitea-event":"pull_request"})"#,
        ),
    },
    ToolDoc {
        name: "bro_webhook_deliveries",
        category: ToolCategory::Workflows,
        summary: "Recent webhook deliveries as a bounded ring buffer (last ~200). Each entry: (received_at, webhook_name, source, headers, extracted_entity, verdict_classification, response_status, response_body). `source` is `webhook` for live deliveries and `replay` for the no-signature replay endpoint. `verdict_classification` echoes how the routing packet classified the event (`start_arc` / `signal_arc` / `cancel_arc` / `ignore` / `dead_letter` / `no_match` / `duplicate_dropped` / `error`). Filter by `name=` (webhook name) and `since=` (ISO timestamp). Replaces poking the upstream code-host's hook-task table or grepping the daemon's tracing log to debug routing-rule misses.",
        when_to_use: "Use when a webhook should have advanced an arc but didn't. Filter to the webhook name and inspect the most recent entry's `extracted_entity` to confirm the extractor projected the right fields, then `verdict_classification` to see what the routing packet decided. `no_match` / `ignore` for an event you expected to route reveals a missing or mis-shaped routing rule. Pair with `bro_signals` to see what (if any) signal made it past routing into the wait-resolution path.",
        example: Some(r#"bro_webhook_deliveries(name="forgejo", limit=10)"#),
    },
    ToolDoc {
        name: "bro_arc_cancel",
        category: ToolCategory::Workflows,
        summary: "Cancel a running workflow arc by id. Trips the arc's cancellation token; the runner observes between node iterations and inside Wait suspensions, bails out with status `cancelled`, runs `on_arc_cancel` (if declared) followed by `on_arc_exit`, and writes a `blocked` note (`workflow cancelled`) on the arc's thread. Returns `{cancelled: true|false}` — false means no token registered for that arc id (already terminated, never started, or wrong id).",
        when_to_use: "Use to manually stop a runaway, mis-dispatched, or no-longer-relevant arc without restarting the daemon. Pair with `bro_arc_status` to find the arc id first. Cleanup hooks fire automatically — workflows with worktree creation in `on_arc_exit` will tear down the worktree on cancel just as on success.",
        example: Some(r#"bro_arc_cancel(arc_id="arc-9116dad8465043a39987a76cfa22108a")"#),
    },
    ToolDoc {
        name: "bro_signals",
        category: ToolCategory::Workflows,
        summary: "Recent signal-dispatch events as a bounded ring buffer (last ~200). Every call to the signal router records one entry: (timestamp, signal, correlation, outcome, matched_arc_id, matched_wait_id, idle_pending). `outcome` is `matched` (resolved a wait) or `no_matching_wait` (fell idle); on idle, `idle_pending` carries the pending-with-same-signal snapshot at dispatch time so the diff between what arrived and what was waiting is one read away. Filter by `signal=` (exact match) and `since=` (ISO timestamp). Replaces the journalctl|grep workflow for debugging webhook → routing → signal → wait paths.",
        when_to_use: "Use when an arc is parked on a Wait and you can't tell why a webhook didn't resolve it. Filter to the signal name in question and look at the most recent entries — `no_matching_wait` with `idle_pending` showing the right wait + a different correlation diff means the routing rule emitted the wrong correlation type/value. `matched` entries confirm the resolution path is wired.",
        example: Some(r#"bro_signals(signal="pr-ready", limit=20)"#),
    },
    ToolDoc {
        name: "bro_webhook_install",
        category: ToolCategory::Workflows,
        summary: "Install a webhook endpoint reachable at POST /webhook/<name>. Signature verification, extractor projection, and routing-packet dispatch are mechanical at the daemon. Routing packets must already be operator-installed in the global packet store.",
        when_to_use: "Use to wire an external event source (Forgejo, GitHub, Stripe, generic JSON poster) into the workflow engine. Spec carries: name, signature scheme + secret env var, Extractor, routing packet id, optional delivery-id header for idempotency dedup. Persisted to disk for restart durability.",
        example: Some(
            r#"bro_webhook_install(spec={"name":"forgejo","signature":{"kind":"forgejo","secret_env":"FORGEJO_WEBHOOK_SECRET"},"extractor":{...},"routing_packet":"packet-abc"})"#,
        ),
    },
    ToolDoc {
        name: "bro_webhook_list",
        category: ToolCategory::Workflows,
        summary: "List installed webhooks as name-ordered summary pages (default 20, maximum 100). Filter by exact name and continue with next_offset. detail=true adds safe configuration diagnostics; credentials, opaque URL components, payload values, selector constants, and server-local paths stay omitted.",
        when_to_use: "Inventory check — what webhooks does this daemon serve? Useful before installing to avoid duplicate names.",
        example: Some("bro_webhook_list()"),
    },
    ToolDoc {
        name: "bro_webhook_remove",
        category: ToolCategory::Workflows,
        summary: "Remove an installed webhook by name: drops it from the in-memory registry (POST /webhook/<name> starts 404ing immediately) and deletes its persisted spec file so it does not reload on daemon restart. Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the webhook fully installed rather than half-removed.",
        when_to_use: "Decommission a webhook endpoint that's no longer needed, or clear one before reinstalling a corrected spec under the same name. Errors if the name isn't currently installed.",
        example: Some(r#"bro_webhook_remove(name="forgejo")"#),
    },
    ToolDoc {
        name: "bro_poller_install",
        category: ToolCategory::Workflows,
        summary: "Install a scheduled HTTP-source poller that converges on the same routing pipeline as webhook ingress. Use when the upstream doesn't push (no webhook capability) or the daemon has no public ingress. Spec carries: name, every_seconds (>= BBOX_POLLER_MIN_INTERVAL_SECS, default 5), source (HttpFetchSpec), optional iterate (Selector — array path to explode response into N events), per-event extractor, optional dedup_id_path (Selector for stable id, in-memory recent-seen ring per poller), routing_packet, optional default_project_dir. Persisted to disk + tick loop spawned immediately; reinstall replaces the running task.",
        when_to_use: "Use when there's no webhook (closed-network upstream, no public ingress on the daemon, polling-only API) or when a clock-driven trigger is what you actually want. Routing packet evaluates against per-item extracted entity exactly the way webhook ingress does — same dispatch_routed_event entry point.",
        example: Some(
            r#"bro_poller_install(spec={"name":"forgejo-issues","every_seconds":120,"source":{"url":"http://127.0.0.1:3000/api/v1/repos/owner/repo/issues?state=open","headers":{"Authorization":"token ..."}},"iterate":{"kind":"json_path","path":"$"},"extractor":{...},"dedup_id_path":{"kind":"json_path","path":"$.id"},"routing_packet":"domain:webhook-routing/forgejo"})"#,
        ),
    },
    ToolDoc {
        name: "bro_poller_list",
        category: ToolCategory::Workflows,
        summary: "List installed pollers as name-ordered summary pages (default 20, maximum 100). Filter by exact name and continue with next_offset. detail=true adds safe configuration diagnostics; credentials, opaque URL components, payload values, selector constants, and server-local paths stay omitted.",
        when_to_use: "Inventory check before installing to avoid duplicate names; also surfaces effective tick intervals (which may be clamped above your configured value via BBOX_POLLER_MIN_INTERVAL_SECS).",
        example: Some("bro_poller_list()"),
    },
    ToolDoc {
        name: "bro_poller_remove",
        category: ToolCategory::Workflows,
        summary: "Remove an installed poller by name: aborts its running tick-loop task immediately, clears its dedup ring, and deletes its persisted spec file so it does not respawn on daemon restart. Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the poller fully installed rather than half-removed.",
        when_to_use: "Decommission a poller that's no longer needed, or clear one before reinstalling a corrected spec under the same name. Errors if the name isn't currently installed.",
        example: Some(r#"bro_poller_remove(name="forgejo-issues")"#),
    },
    ToolDoc {
        name: "bro_cron_install",
        category: ToolCategory::Workflows,
        summary: "Install a calendar-driven cron inlet — sibling of webhook + poller. Same routing pipeline (extractor → routing packet → dispatch_routed_event), different trigger source: wall-clock schedule, no fetch. Spec: name, schedule (6-field cron expr `sec min hour dom mon dow`), optional payload (operator-supplied entity fields), optional concurrency cap (default 1, set 0 to disable), routing_packet, optional default_project_dir. Synthetic entity fields `cron_name` + `tick_at` are merged in at tick time so routing rules can discriminate.",
        when_to_use: "Use when the trigger is time-based, not event-based — nightly maintenance arcs, hourly health sweeps, weekly report generation, scheduled SAST squashing. The dispatched arc is responsible for any data acquisition (typically via mcp_call hooks) since cron carries no fetch. Concurrency cap of 1 (default) skips ticks while a prior arc is still in flight — set higher for bursty parallelism, set 0 to lift the cap entirely.",
        example: Some(
            r#"bro_cron_install(spec={"name":"sastquatch-daily","schedule":"0 0 9 * * *","payload":{"owner":"sastquatch","repo":"demo"},"concurrency":1,"routing_packet":"domain:cron-routing/sastquatch"})"#,
        ),
    },
    ToolDoc {
        name: "bro_cron_list",
        category: ToolCategory::Workflows,
        summary: "List installed crons as name-ordered summary pages (default 20, maximum 100). Filter by exact name and continue with next_offset. detail=true adds safe configuration diagnostics; credentials, opaque URL components, payload values, selector constants, and server-local paths stay omitted.",
        when_to_use: "Inventory check before installing; also surfaces in-flight count so you can tell whether a cap is currently blocking a tick.",
        example: Some("bro_cron_list()"),
    },
    ToolDoc {
        name: "bro_cron_remove",
        category: ToolCategory::Workflows,
        summary: "Remove an installed cron by name: aborts its running tick loop immediately, clears in-flight concurrency state, and deletes the persisted spec file so it does not respawn on daemon restart. Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the cron fully installed rather than half-removed. A cron installed via bbox_artifact_install (kind=\"cron\") is catalog-managed and gets re-materialized on the next catalog sync; remove it with bbox_artifact_remove instead.",
        when_to_use: "Decommission a cron inlet that's no longer needed, or clear one before reinstalling a corrected spec under the same name. Errors if the name isn't currently installed.",
        example: Some(r#"bro_cron_remove(name="sastquatch-daily")"#),
    },
    ToolDoc {
        name: "bro_cron_upcoming",
        category: ToolCategory::Workflows,
        summary: "Compute the next N scheduled times for a cron expression as RFC3339 strings. Pure function — does not touch the registry.",
        when_to_use: "Validate a schedule before installing or eyeball when a live cron will fire next. Useful for human review of `0 */15 * * * *` etc. without booting a daemon.",
        example: Some(r#"bro_cron_upcoming(schedule="0 0 9 * * *", count=3)"#),
    },
    ToolDoc {
        name: "bro_workflow_install",
        category: ToolCategory::Workflows,
        summary: "Install a workflow spec by id so it can be referenced by name from webhook routing verdicts (`{route: start_arc, workflow: <id>}`) and other lookup paths. Compile-validated before install; capability tags enforced. `subworkflow_ref` seams are validated against already-installed children (required imports covered, exports declared in the child schema); refs not installed yet come back as `warnings` rather than refusals so install order stays free.",
        when_to_use: "Persist a workflow that webhooks or scheduled triggers will dispatch by name. Install alongside the routing packet that emits `start_arc` verdicts referencing this id.",
        example: Some(r#"bro_workflow_install(id="issue-to-pr", spec={...full Workflow JSON...})"#),
    },
    ToolDoc {
        name: "bro_workflow_list",
        category: ToolCategory::Workflows,
        summary: "List installed workflows as name/version summary pages (default 20, maximum 100), ordered by registry name. Filter by exact name and continue with next_offset. detail=true adds entry node, node/actor counts, and policy packet; workflow hooks and embedded credentials are never returned.",
        when_to_use: "Inventory check — what workflows can routing verdicts target on this daemon?",
        example: Some("bro_workflow_list()"),
    },
    ToolDoc {
        name: "bro_workflow_remove",
        category: ToolCategory::Workflows,
        summary: "Remove an installed workflow by registry id: deletes it from the registry and its persisted spec file so webhook/poller/cron routing verdicts and subworkflow_ref lookups can no longer resolve it. Refuses when any running_arcs entry is still non-terminal (status \"running\") for either this registry id or the resolved spec's own name, unless force=true. Does not cancel or otherwise touch arcs already dispatched from this workflow (use bro_arc_cancel for that). Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the workflow fully installed rather than half-removed. A workflow installed via bbox_artifact_install (kind=\"workflow\") is catalog-managed and gets re-materialized on the next catalog sync; remove it with bbox_artifact_remove instead.",
        when_to_use: "Decommission or replace an installed workflow. Leave force unset in normal operation so an in-flight arc can't be orphaned mid-run; pass force=true only when you've confirmed the running arc(s) should be abandoned (they keep running, but the id can no longer be re-dispatched by name).",
        example: Some(r#"bro_workflow_remove(id="issue-to-pr")"#),
    },
    // ── Agents ──────────────────────────────────────────────────
    ToolDoc {
        name: "bro_agent_list",
        category: ToolCategory::Orchestration,
        summary: "List installed agents in name/version order as compact summary pages (default 20, maximum 100). Continue with next_offset. Existing registry filters apply before paging. detail=true expands descriptions and installation diagnostics; bro_agent_get/bro_agent_describe reads one exact agent.",
        when_to_use: "Discover what agents are available for dispatch, composition, or review. Filter by cost_class to find cheap/expensive agents; use include_superseded=true to see version history.",
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
        summary: "Full manifest + resolved brofile + merged filters for one agent. Returns the computed dispatch surface (deny-wins filter merge of brofile + overlay), brofile info, embedding status, and any warnings.",
        when_to_use: "Pre-dispatch inspection: understand the full computed tool surface an agent will have at runtime, including which brofile it resolves to and whether filters from the brofile and manifest overlay conflict. Use before bro_agent_dispatch to preview the dispatch plan.",
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
        summary: "Dispatch a registered agent for a focused task. Routes through manifest dispatch_adapter if set, otherwise resolves brofile, merges filters, expands prompt template, and spawns via the standard bro execution path. Returns task_id, session, and agent attribution (agentLabel on the spawned task, preserved even when bro= routes to a named team member).",
        when_to_use: "Dispatching an agent after discovery via bro_agent_search. Returns (task_id, session) — resume with bro_resume, status with bro_status. Prefer over hand-rolling a brofile + bro_exec when the task matches an agent's description and when_to_use. Set the session's working directory with `cwd` (canonical; `project_dir` accepted as a deprecated alias). Pass runtime={...} to overlay tier/pool/pin allocation on the standard bro dispatch path. Recursion is manifest-declared: an agent whose manifest sets allow_recursion=true dispatches with the recursive bro_* tools available; there is no per-call override (ad-hoc recursive dispatch is bro_exec's allow_recursion). Anti-pattern: do not dispatch when the agent's manifest declares one of your task's properties as an anti_pattern.",
        example: Some(
            r#"bro_agent_dispatch(agent="code-reviewer", cwd="/repo/x", args={"diff": "..."})"#,
        ),
    },
    // ── Atoms ───────────────────────────────────────────────────
    ToolDoc {
        name: "atom_list",
        category: ToolCategory::Orchestration,
        summary: "List installed atoms in name/version order as compact summary pages (default 20, maximum 100). Continue with next_offset. Existing registry filters apply before paging. detail=true expands descriptions and installation diagnostics; atom_get/atom_describe reads one exact atom.",
        when_to_use: "Discover what typed capabilities (atoms) are available. Filter by subcontract to find e.g. refactor atoms (subcontract=\"refactor/v1\"). Use include_superseded=true to see version history.",
        example: Some(r#"atom_list(subcontract="refactor/v1")"#),
    },
    ToolDoc {
        name: "atom_get",
        category: ToolCategory::Orchestration,
        summary: "Read full details for a single atom by name or atom-ref (atom:name@vN, atom:name@latest, or bare name). Returns manifest, metadata, lifecycle state, and subcontract.",
        when_to_use: "Inspect a specific atom's manifest (implementation, effects, composition, inputs/outputs, supervision, trace) before invoking or composing it.",
        example: Some(r#"atom_get(name="atom:rust-test-island-extract@v1")"#),
    },
    ToolDoc {
        name: "atom_describe",
        category: ToolCategory::Orchestration,
        summary: "Full manifest + implementation details for one atom. Returns the complete manifest including effects, composition constraints, supervision policy, and any install warnings.",
        when_to_use: "Pre-invocation inspection: understand the full contract an atom exposes — what it accepts, what it returns, what effects it may perform, and which atoms it may invoke.",
        example: Some(r#"atom_describe(atom="rust-test-island-extract")"#),
    },
    ToolDoc {
        name: "atom_search",
        category: ToolCategory::Orchestration,
        summary: "Search installed atoms by query string. Matches against description and when_to_use; penalizes or excludes results matching anti_patterns. Returns ranked results with scores, provenance, and v1 route-card fields: handle, kind, fit, next, missing_facts, stop_if.",
        when_to_use: "Discovery: find atoms relevant to a task. Call with the task description to get ranked candidates. Set exclude_anti_pattern_matches=false to see all matches including anti-pattern hits. Filter by cost_class, provenance_kind, or subcontract to narrow results.",
        example: Some(r#"atom_search(query="extract inline tests from Rust files", limit=3)"#),
    },
    // ── Whiteboards ─────────────────────────────────────────────
    ToolDoc {
        name: "atom_invoke",
        category: ToolCategory::Orchestration,
        summary: "Invoke an installed atom. Resolves the atom manifest, validates policy gates (effects, composition, depth), and dispatches via the appropriate implementation path (profile, workflow, deterministic, adapter). Returns an owned invocation handle with invocation_id and underlying task/session ids.",
        when_to_use: "Invoke an atom after discovery via atom_search. Returns invocation_id — check with atom_status, continue with atom_resume, share with atom_delegate. Profile-backed atoms dispatch through the existing bro execution path and accept runtime={...} as a RuntimeRequest overlay.",
        example: Some(
            r#"atom_invoke(atom="atom:rust-test-island-extract@v1", args={"project_dir": "/repo/x", "source_file_or_dir": "src/lib.rs"}, project_dir="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "atom_status",
        category: ToolCategory::Orchestration,
        summary: "Read the status of an atom invocation. Ownership-gated: only owners can read status. Returns a normalized trace envelope with state, timestamps, effects observed, cost, and summary.",
        when_to_use: "Check on an invocation returned by atom_invoke. Pass the owner identity that was set at invoke time. For profile-backed atoms, refreshes status from the underlying bro task.",
        example: Some(r#"atom_status(invocation_id="inv-abc123", owner="operator:claude")"#),
    },
    ToolDoc {
        name: "atom_resume",
        category: ToolCategory::Orchestration,
        summary: "Resume a profile-backed atom invocation. Ownership-gated and only for resumable handles (profile-backed, in a runnable state). Resumes underlying provider session using existing bro resume internals.",
        when_to_use: "Continue a profile-backed atom invocation with a follow-up prompt. Deterministic, adapter, and workflow handles return not_resumable. Must be an owner of the invocation.",
        example: Some(
            r#"atom_resume(invocation_id="inv-abc123", prompt="now run cargo test", owner="operator:claude")"#,
        ),
    },
    ToolDoc {
        name: "atom_delegate",
        category: ToolCategory::Orchestration,
        summary: "Grant another owner access to an atom invocation. Owner-only. v1 does not support revocation. Delegated owners gain full status/resume/delegate rights.",
        when_to_use: "Share atom invocation access with another agent or operator. The grant_to identity becomes a co-owner. Use before atom_status or atom_resume from a different identity.",
        example: Some(
            r#"atom_delegate(invocation_id="inv-abc123", grant_to="operator:codex", owner="operator:claude")"#,
        ),
    },
    // ── Whiteboards ─────────────────────────────────────────────
    ToolDoc {
        name: "whiteboard_open",
        category: ToolCategory::Whiteboards,
        summary: "Open a new whiteboard for structured deliberation. The board collects posts (blind phase), annotations (validate/debate phases), and votes (debate phase) from registered agents, advanced through phases by a facilitator-or-operator role. Returns when the board is created and the opener is registered as facilitator. Idempotent re-open against an existing id is rejected — use whiteboard_state to inspect.",
        when_to_use: "Use to start a deliberation. The opener becomes the facilitator. Pass `arc_thread_id` when the board belongs to a workflow arc — the engine threads it for inbox attribution. External clients (operator's Claude, dispatched help) can open ad-hoc boards by omitting `arc_thread_id`.",
        example: Some(
            r#"whiteboard_open(board_id="adr-2026-04-27", topic="Adopt async runtime X?", opened_by="facilitator")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_register",
        category: ToolCategory::Whiteboards,
        summary: "Register an agent on an existing board. Idempotent — re-registration with the same name is a no-op. Roles: `specialist` (post + annotate + vote), `facilitator` (transition + post + annotate + vote), `operator` (same powers as facilitator; convention is for human / external Claude joiners).",
        when_to_use: "Use to join an open deliberation. Specialists can post in blind, annotate in validate / debate, and vote in debate. Facilitators / operators can additionally transition phases — the only role distinction that matters mechanically.",
        example: Some(
            r#"whiteboard_register(board_id="adr-2026-04-27", agent_name="security", role="specialist", domain="threat-modeling")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_post",
        category: ToolCategory::Whiteboards,
        summary: "Post a structured claim/proposal/concern to a whiteboard during its blind phase. Type one of: proposal, claim, concern, informational. Optional fields target_file / target_location / severity / finding_refs / cascade_targets enable conflict detection downstream.",
        when_to_use: "Use during the blind phase to record your stance. Other agents' posts are not visible to you in blind — that's the point. Once the board transitions to read, everyone sees everything. Severity + finding_refs let `whiteboard_conflicts` surface severity-disagreement conflicts later.",
        example: Some(
            r#"whiteboard_post(board_id="adr-2026-04-27", agent_name="security", type="concern", title="Async runtime increases attack surface", body="...", severity="medium")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_state",
        category: ToolCategory::Whiteboards,
        summary: "Read a bounded visible-board preview (up to five posts, annotations, and votes), with truthful visible counts. Blind hides peer posts; debate specialists see only own or related annotations and own votes. Select post_id to focus one visible post. Read detail=full for exact filtered JSON body pages; continue with cursor=body.next_cursor and parse concatenated body.text. Unknown or invisible post ids return not found.",
        when_to_use: "Use to inspect the board before posting / annotating / voting / transitioning. The `ready_for_transition` flag is advisory only — the facilitator still owns the actual decision. External Claudes joining mid-deliberation start here.",
        example: Some(r#"whiteboard_state(board_id="adr-2026-04-27", agent_name="security")"#),
    },
    ToolDoc {
        name: "whiteboard_annotate",
        category: ToolCategory::Whiteboards,
        summary: "Annotate a post during the validate or debate phase. Validate phase accepts only `validation` (with required `result`: confirmed / refuted / inconclusive). Debate phase accepts `challenge`, `corroborate`, or `resolve` (resolve must reference a challenge id via `resolves`; a post owner may resolve another agent's challenge on their own post).",
        when_to_use: "Use to react to other specialists' posts. You can't challenge or corroborate your own post; `resolve` is the exception for answering another agent's challenge on your own post. `challenge` says you disagree (typically with reasoning), `corroborate` adds supporting evidence, `resolve` closes a challenge with a position. The challenge → resolve graph is what `ready_for_transition` checks in debate phase.",
        example: Some(
            r#"whiteboard_annotate(board_id="adr-2026-04-27", agent_name="perf", post_id="post-001", type="challenge", body="missing runtime cost analysis under load")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_vote",
        category: ToolCategory::Whiteboards,
        summary: "Cast an advisory vote on a post during the debate phase. One vote per agent per post — re-vote replaces. Vote: accept, reject, or defer.",
        when_to_use: "Use to record your position on each post during debate. Tallies are exposed via `whiteboard_summarize` and via the `board.vote_tally.<post_id>` template scope inside workflows. Gate packets in workflows can branch on tally shape (e.g. supermajority accept → merge).",
        example: Some(
            r#"whiteboard_vote(board_id="adr-2026-04-27", agent_name="design", post_id="post-001", vote="accept", reason="aligns with the durable-state principle")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_transition",
        category: ToolCategory::Whiteboards,
        summary: "Advance the board to a new phase. Facilitator or operator role required. Sequence: blind → read → validate → debate → resolve → archived; read → debate and validate → resolve are legal skips. Transition emits a `board-transitioned` signal correlated to (board_id, target_phase) so any wait node observing the board resumes.",
        when_to_use: "Use to advance the deliberation when ready. Check `whiteboard_state.ready_for_transition` first as an advisory. Workflows can define a wait-on-phase node that resumes when the transition fires — this is how the engine drives multi-phase arcs through the board.",
        example: Some(
            r#"whiteboard_transition(board_id="adr-2026-04-27", agent_name="facilitator", target_phase="debate", summary="all specialists posted; advancing")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_conflicts",
        category: ToolCategory::Whiteboards,
        summary: "Auto-detect conflicts between posts on a board. Returns three kinds: `direct_overlap` (same target_file + identical target_location), `cascade_collision` (post A cascades to post B's direct target), `severity_disagreement` (same finding_ref, distinct severities). Available in any phase past blind. Default returns at most ten conflict previews and the total count. detail=full returns exact JSON body pages; follow body.next_cursor.",
        when_to_use: "Use during read / validate / debate to surface what specialists disagree on or where their proposed actions collide. The facilitator typically reviews this before transitioning to debate so contested points get explicit annotations.",
        example: Some(
            r#"whiteboard_conflicts(board_id="adr-2026-04-27", agent_name="facilitator")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_summarize",
        category: ToolCategory::Whiteboards,
        summary: "Summarize only the requesting agent's visible evidence: exact counts and readiness, with bounded post-standing, vote-tally, and agent previews. Gate counts remain numeric and complete for that visible scope. detail=full returns the complete visible summary as JSON body pages; follow body.next_cursor. Hidden peer evidence never contributes counts or ids.",
        when_to_use: "Use for a quick read of board state without paying the full post-body cost. Good for inbox views, gate-packet entity inputs, and long-running observers (e.g. a polling external Claude).",
        example: Some(
            r#"whiteboard_summarize(board_id="adr-2026-04-27", agent_name="facilitator")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_archive",
        category: ToolCategory::Whiteboards,
        summary: "Archive the board (facilitator/operator role, same authority as a phase transition). Resolve phase only, unless force=true, the abandon path for boards stranded mid-phase by a failed arc. Removes the board from active deliberation and returns archive summary statistics.",
        when_to_use: "Use after the deliberation completes and any synthesis artifact (ADR markdown, PR body, etc.) has been produced. Use force=true from cleanup hooks (e.g. on_arc_exit) when a failed arc stranded the board mid-phase. Archived boards stay readable on disk for audit but no longer count toward inbox attention.",
        example: Some(r#"whiteboard_archive(board_id="adr-2026-04-27", agent_name="facilitator")"#),
    },
    // ── Workspace tools ──────────────────────────────────────────────
    ToolDoc {
        name: "work_tool_calls",
        category: ToolCategory::Workspace,
        summary: "Query indexed workspace tool-call records by server, tool_name, glob_pattern, tool_kind, target, project, and time. Rows preserve (server, tool_name) identity.",
        when_to_use: "Use to answer recent tool-use questions. Filter by server + tool_name for one tool, or glob_pattern for families like `work_git_*`.",
        example: Some(
            r#"work_tool_calls(server="blackbox", tool_kind="bash", project="/repo/x", limit=20)"#,
        ),
    },
    ToolDoc {
        name: "work_smart_read",
        category: ToolCategory::Workspace,
        summary: "Read a file with stable line numbers and optional bounded bbox overlays. Use offset/limit to window large files; set enrich=false for plain reads.",
        when_to_use: "Use instead of raw Read on registered-project files when line citations or related notes may matter.",
        example: Some(r#"work_smart_read(file_path="/repo/x/src/main.rs", offset=0, limit=100)"#),
    },
    ToolDoc {
        name: "work_bash",
        category: ToolCategory::Workspace,
        summary: "Run a shell command in an explicit cwd with streaming progress chunks, 32KB stdout/stderr final caps, and timeout handling.",
        when_to_use: "Use instead of raw Bash only when workspace-tools mode is explicitly enabled for the dispatch. `cwd` is required; `task_id` correlates the call to a dispatch task.",
        example: Some(
            r#"work_bash(command="cargo test --bin blackboxd", cwd="/repo/x", timeout_secs=120)"#,
        ),
    },
    ToolDoc {
        name: "work_git_status",
        category: ToolCategory::Workspace,
        summary: "Structured git status for a repository: branch, staged/unstaged/untracked files, and clean flag.",
        when_to_use: "Use before committing or dispatching a writer; the clean flag is easy to gate on.",
        example: Some(r#"work_git_status(repo="/repo/x")"#),
    },
    ToolDoc {
        name: "work_git_log",
        category: ToolCategory::Workspace,
        summary: "Structured git commit log with sha, parents, author, date, and subject. Default limit is 20, max 200.",
        when_to_use: "Use for branch orientation before edits; JSON output is easier to parse than raw git log.",
        example: Some(r#"work_git_log(repo="/repo/x", limit=10)"#),
    },
    ToolDoc {
        name: "work_git_diff",
        category: ToolCategory::Workspace,
        summary: "Structured git diff for working tree or staged changes, optionally path-restricted, with 32KB output cap. Set include_untracked=true to include untracked files as new-file patches.",
        when_to_use: "Use to review changes before committing or verify that an edit produced the expected delta. Set include_untracked=true during closeout review when new files may not be staged yet.",
        example: Some(r#"work_git_diff(repo="/repo/x", staged=true, include_untracked=true)"#),
    },
    ToolDoc {
        name: "work_git_show",
        category: ToolCategory::Workspace,
        summary: "Show one commit by hex SHA with metadata and diff, capped at 32KB.",
        when_to_use: "Use to inspect a specific commit returned by `work_git_log`. SHA is validated as hex-only before use to prevent injection.",
        example: Some(r#"work_git_show(repo="/repo/x", sha="abc123def456")"#),
    },
    ToolDoc {
        name: "work_git_commit",
        category: ToolCategory::Workspace,
        summary: "Stage and commit files, rejecting sensitive paths before staging. Omitting files stages tracked modifications only; never pushes.",
        when_to_use: "Use when an executor needs to commit. Supply `task_id` for a done note; prefer explicit files for scoped changes.",
        example: Some(
            r#"work_git_commit(repo="/repo/x", message="fix: correct off-by-one in parser", files=["src/parser.rs"], task_id="task-abc")"#,
        ),
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
        summary: "Dry-run or apply edge sidecar garbage collection. Reports exact candidates with path, bytes, and rule for temps, backups, orphan classes, inactive snapshots, and observed cap warnings.",
        when_to_use: "Use for sidecar cleanup after inspecting health. Defaults retain newest backups, recent inactive snapshots per workspace/repo, branch-switch grace, and keep observed history. Inactive-snapshot age retention is bounded by per-workspace count/byte budgets (`max_snapshots_per_workspace`, default 32; `max_snapshot_total_bytes_per_workspace`, default 16 GiB) so under-age snapshots still prune once a workspace exceeds the budget — floors always win. `dangling_path` and `legacy_unknown` orphans can auto-prune after grace; `explicitly_unregistered` requires `prune_explicitly_unregistered=true`. `prune_duplicate_packets=true` additionally dedupes byte-identical rule-packet copies (keeps the newest per domain/scope/project; Apply-referenced copies are protected).",
        example: None,
    },
    ToolDoc {
        name: "bbox_storage_migrate_legacy_edges",
        category: ToolCategory::StorageHealth,
        summary: "Dry-run or apply legacy edge sidecar migration into lifecycle-owned explicit/observed lanes. Drops derived only when managed replacement exists; quarantines malformed lines.",
        when_to_use: "Migrating pre-Phase-2 legacy sidecars into lane-split storage.",
        example: None,
    },
    // ── System Events ────────────────────────────────────────────────
    ToolDoc {
        name: "system_event_emit",
        category: ToolCategory::Orchestration,
        summary: "Emit a synthetic system event into the journal and broadcast. Ops-only; surface-enforced.",
        when_to_use: "Use for testing, manual event injection, or operational triggers. Accepts kind, producer, optional project, causation_id, principal, subject, correlation, and payload — same shape as the typed SystemEvent. Production identity events come from `require_identity`, not this tool. Requires ops surface.",
        example: Some(
            r#"system_event_emit(kind="bro.identity.required", producer="manual-test", principal={"kind":"bro","bro":"keystone-review","provider":"claude","model":"claude-haiku-4-5-20251001"}, subject={"kind":"bro","id":"bro:keystone-review"}, payload={"identity_scope":"forgejo","instance":"local-forgejo15"})"#,
        ),
    },
    ToolDoc {
        name: "system_event_compact",
        category: ToolCategory::Orchestration,
        summary: "Apply system-event journal and outbox retention compaction. Ops-only; surface-enforced.",
        when_to_use: "Use for manual ops compaction or from the daily-compaction workflow. Applies the same retention as startup compaction: event journal age/count retention plus succeeded-outbox retention, and returns before/after/drop counts.",
        example: Some(r#"system_event_compact()"#),
    },
    ToolDoc {
        name: "system_event_list",
        category: ToolCategory::Orchestration,
        summary: "List journal event summaries newest first (default 20, maximum 100). Continue with next_before as before; keep filters unchanged. A missing/compacted cursor errors. Filters match recorded kind/producer/project tags exactly. Payload, correlation, principals, and host project paths are omitted; system_event_open(event_id) expands one event.",
        when_to_use: "Use to inspect recent events, filter by kind/producer/project. Does not leak resolved secret values.",
        example: Some(r#"system_event_list(kind="task.completed", limit=10)"#),
    },
    ToolDoc {
        name: "system_event_open",
        category: ToolCategory::Orchestration,
        summary: "Open a single system event with causation chain and derived event links. Readonly.",
        when_to_use: "Use to inspect a specific event and its causal ancestry or derived children.",
        example: Some(r#"system_event_open(event_id="evt-abc123")"#),
    },
    // ── Reactions ──────────────────────────────────────────────────
    ToolDoc {
        name: "reaction_install",
        category: ToolCategory::Orchestration,
        summary: "Install a reaction spec. Ops-only. Validates and persists to disk.",
        when_to_use: "Use to register a new reaction that subscribes to system event kinds and triggers actions. Requires replace=true to overwrite.",
        example: Some(
            r#"reaction_install(spec={"_contract":"reaction/v1","name":"my-react",...}, replace=false)"#,
        ),
    },
    ToolDoc {
        name: "reaction_list",
        category: ToolCategory::Orchestration,
        summary: "List reactions in name order as summary pages (default 20, maximum 100). Filter exact name; continue with next_offset. detail=true expands event kinds and retry/failure policies, never action arguments or credentials. warning_count reports invalid stored specs; view=warnings pages safe warning names and categories without host paths.",
        when_to_use: "Use to see which reactions are installed and their event_kinds, action, and enabled status.",
        example: Some(r#"reaction_list()"#),
    },
    ToolDoc {
        name: "reaction_replay",
        category: ToolCategory::Orchestration,
        summary: "Dry-run replay a reaction against an event. Returns rendered outputs without executing side effects.",
        when_to_use: "Use to preview what a reaction would do: rendered idempotency key, gate decision, action args with secrets redacted.",
        example: Some(
            r#"reaction_replay(mode="dry_run", event_id="evt-abc123", reaction="my-react")"#,
        ),
    },
    ToolDoc {
        name: "reaction_execute",
        category: ToolCategory::Orchestration,
        summary: "Execute a reaction once against an event through the audited outbox path. Ops-only. Set force=true to bypass succeeded-idempotency suppression.",
        when_to_use: "Use after a dry-run when an operator needs to execute or force-reexecute a reaction against a specific event. The tool creates and claims an outbox row, runs normal gates/guards/action execution, and persists the final delivery status.",
        example: Some(
            r#"reaction_execute(event_id="evt-abc123", reaction="my-react", force=false)"#,
        ),
    },
    ToolDoc {
        name: "reaction_deliveries",
        category: ToolCategory::Orchestration,
        summary: "List outbox delivery records with optional filters.",
        when_to_use: "Use to inspect outbox records — pending, claimed, succeeded, retry, dead-lettered. Filter by event_id or status.",
        example: Some(r#"reaction_deliveries(status="dead_lettered")"#),
    },
    ToolDoc {
        name: "reaction_retry",
        category: ToolCategory::Orchestration,
        summary: "Retry a dead-lettered outbox record. Ops-only. Requires explicit outbox id.",
        when_to_use: "Use to requeue a specific dead-lettered record after investigating and fixing the root cause.",
        example: Some(r#"reaction_retry(outbox_id="outbox-abc123")"#),
    },
    // ── Identity ─────────────────────────────────────────────────────
    ToolDoc {
        name: "identity_list",
        category: ToolCategory::Orchestration,
        summary: "List all durable external identity mappings. Readonly.",
        when_to_use: "Use to inspect provisioned bro-to-external-user mappings across all scopes and instances.",
        example: Some(r#"identity_list()"#),
    },
    ToolDoc {
        name: "identity_get",
        category: ToolCategory::Orchestration,
        summary: "Get a single external identity mapping by (scope, instance, subject, provider, model). Readonly.",
        when_to_use: "Use to look up the provisioned external identity for a specific bro/provider/model triple on a given instance.",
        example: Some(
            r#"identity_get(scope="forgejo", instance="local-forgejo15", subject="bro:keystone-review", provider="claude", model="claude-haiku-4-5-20251001")"#,
        ),
    },
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
- **Executor** — when running as a dispatched bro/task/workflow/atom actor, \
does the work and returns its result. `bbox_note` is available for *notable* \
observations worth surfacing to the orchestrator (`dispute`, `surprise`, \
`blocked`, `learned`, actionable `followup`) — it is not a per-dispatch ritual, \
so stay silent when nothing is notable. A caller that needs a structured \
sign-off (an atom, workflow, or explicit completion contract) may require a \
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
- Workflow vs. manual dispatch: when you're about to author (or \
re-author) a multi-phase protocol with gates, retries, or ensemble \
review — reach for `bro_orchestrate_run` with a mermaid-shaped spec \
instead of pasting a discipline-protocol into an LLM and hoping it \
won't drift. The daemon owns the state machine; the LLM is a turn. \
See `sm-workflow-orchestration` via `bbox_knowledge`.
- `bbox_learn` is for operator-approved, user-stated rules; `bbox_note(kind=learned)` is for \
agent-discovered facts.
- For long-running bro work, instruct dispatched agents and workflow hook nodes \
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
            (ToolCategory::Workflows, "sm-workflow-orchestration"),
            (ToolCategory::Whiteboards, "sm-whiteboards"),
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
        assert!(md.contains("sm-whiteboards"));
        assert!(md.contains("bbox_knowledge(query=\"sm-whiteboards\")"));
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
