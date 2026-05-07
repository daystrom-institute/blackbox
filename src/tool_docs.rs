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

use crate::knowledge::{Approval, Category, KnowledgeEntry, Priority, Scope, Status};

pub const TOOL_DOC_ENTRY_ID: &str = "bb-tool-reference";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Transcripts,
    Graph,
    Projects,
    Refactor,
    Knowledge,
    Threads,
    Notes,
    Inbox,
    Artifacts,
    Packets,
    Orchestration,
    Workflows,
    Whiteboards,
    Councils,
}

impl ToolCategory {
    fn heading(&self) -> &'static str {
        match self {
            Self::Transcripts => "Transcripts",
            Self::Graph => "Agentic graph",
            Self::Projects => "Projects",
            Self::Refactor => "Refactor mechanization",
            Self::Knowledge => "Knowledge",
            Self::Threads => "Threads",
            Self::Notes => "Side-channel notes",
            Self::Inbox => "Attention / inbox",
            Self::Artifacts => "Artifact catalog",
            Self::Packets => "Rule-packets",
            Self::Orchestration => "Bro orchestration",
            Self::Workflows => "Workflow orchestration",
            Self::Whiteboards => "Whiteboards",
            Self::Councils => "Councils",
        }
    }

    fn intro(&self) -> &'static str {
        match self {
            Self::Transcripts => {
                "Search and read across every Claude Code / Codex / Gemini session the host has recorded. Reach for these when the user asks about past conversations, when you need to cite the origin of a rule, or when you need context around a prior decision."
            }
            Self::Graph => {
                "Inspect entities, graph vocabulary, paths, bundles, and retrieval."
            }
            Self::Projects => "Register project roots for later file indexing.",
            Self::Refactor => {
                "Mechanize structural refactors with tree-sitter-backed source inventory, dry-run extraction plans, hash-checked apply, and parse validation. Inspection is multi-language for supported grammars; mutation starts with Rust item extraction. Pull `sm-refactor` first, then the language runbook: `sm-refactor-rust`, `sm-refactor-typescript`, or `sm-refactor-csharp` via `bbox_knowledge`. These tools are syntax-aware, not semantic rename engines; use language servers or compiler feedback for reference resolution and import repair."
            }
            Self::Knowledge => {
                "Memory lanes: `bbox_learn` for rendered rules, `bbox_remember` for cold recall, `bbox_decide` for durable commitments, and `bbox_pin` for scoped active context."
            }
            Self::Threads => {
                "Track non-dispatchable work that spans sessions (investigations, QC walks, debugging, refinement loops). Lighter than the full dispatch pipeline, heavier than memory. Use `kind=work_item` for orchestrator-led propose→execute→review→refine loops."
            }
            Self::Notes => {
                "Structured side channel for observations emitted during work. Executors call `bbox_note` throughout a dispatch; orchestrators query `bbox_notes` / `bbox_inbox` at round boundaries. Seven kinds: `dispute`, `assumption`, `surprise`, `followup`, `blocked`, `learned`, `done`. The *done* kind with a one-line acceptance summary is the single highest-leverage signal — always emit it on completion."
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
                "Dispatch agents across providers (Claude, GLM, DeepSeek, Inception, Codex, Copilot, Vibe, Gemini). Prefer named `bro` targeting (resolves provider + account + lens + session automatically) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again — it starts fresh with no memory). For ensembles: `bro_broadcast` + `bro_when_all` (blind deliberation) or `bro_when_any` (race)."
            }
            Self::Workflows => {
                "Define multi-phase agent protocols as JSON specs with per-node `next` transitions and dispatch them as a unit. The daemon owns the state machine; actors (executor / ensemble) are dispatched INTO the loop as stateless turns — persona / role / contract is the brofile lens, not an engine type. Gate packets route choice nodes by verdict; retry ceilings cap back-edges; fork + `late_inject` express async steering; sub-workflows compose arcs like rule-packets compose via `Apply`; workflow-level `policy_packet` mechanizes arc-health decisions without an LLM advisor. Whiteboards (see `whiteboard_*` tools) provide multi-agent deliberation with phases + structured posts; `wait_for_phase` resumes arcs on board transitions. Every run opens a `bbox_thread(kind=work_item)` with structured notes + rolling compaction anchors. Replaces long skill-prose protocols (overmind, crucible). See `sm-workflow-orchestration` via `bbox_knowledge` for the full runbook and `examples/workflows/` for the catalog."
            }
            Self::Whiteboards => {
                "Multi-agent deliberation surface. Posts (proposals / claims / concerns / informational), annotations (challenge / corroborate / resolve / validation), and votes accumulate on a board, advanced through phases (blind → read → validate → debate → resolve → archived) by a facilitator-or-operator role. Three audiences share one surface: in-workflow ensemble specialists (their structured outputs auto-post when the node has a `board:` field), in-workflow facilitators (single bro, drives transitions), and external agents — operator's Claude session, dispatched help, eventually humans through slack / ntfy adapters — that read state via `whiteboard_state` and act via `whiteboard_post` / `whiteboard_vote` / `whiteboard_transition`. Phase transitions emit `board-transitioned` signals through the same `dispatch_routed_event` pipeline webhooks use; arcs `wait_for_phase` to resume when the board advances. Replaces phaser as a peer external MCP server."
            }
            Self::Councils => {
                "Multi-peer chat councils — TUI-driven conversational coordination over a team. Read-only MCP surface for external observers; the human-facing CRUD lives in the `bro council` CLI. Distinct from whiteboards: a council is a chat log (turns, @-mentions, riffing), not a structured decision artifact. If a deliberation produces a claim worth durable record, post it to a whiteboard separately. Councils compose with whiteboards; they do not replace them."
            }
        }
    }
}

fn deferred_system_memory(category: ToolCategory) -> Option<&'static str> {
    match category {
        ToolCategory::Packets => Some("sm-rule-packets"),
        ToolCategory::Refactor => Some("sm-refactor"),
        ToolCategory::Orchestration => Some("sm-bro-dispatch-patterns"),
        ToolCategory::Workflows => Some("sm-workflow-orchestration"),
        ToolCategory::Whiteboards => Some("sm-whiteboards"),
        _ => None,
    }
}

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
        name: "bbox_search",
        category: ToolCategory::Transcripts,
        summary: "Search across all indexed transcripts. Default `mode=smart` broadens adjacent terms for recall; `mode=fulltext` gives raw Tantivy/Lucene-style boolean syntax.",
        when_to_use: "Use when you know the topic but not the exact session. Filter by account, project, or role early. Pass `exclude_self=true` for current-turn searches. See `sm-transcript-retrieval` for ladders.",
        example: Some(r#"bbox_search(query="redis locking", project="my-app", role="user")"#),
    },
    ToolDoc {
        name: "bbox_hybrid_search",
        category: ToolCategory::Graph,
        summary: "Hybrid BM25+vector search over typed entities. vector_weight=0.6 by default; set 0.0 for BM25-only behavior, 1.0 for vector-only.",
        when_to_use: "Step 2 of the agentic opening sequence (`sm-agentic-opening-sequence`). Use as the default search for any topical question. Pass `project=$cwd` (or a registered project_id) when querying about your local repo to avoid cross-project keyword pollution. Trust topical hits — top seed is canonical for the query even when wording doesn't exactly match (vector lane catches paraphrases). The query language: adjacent terms broaden recall, quoted phrases stay exact, `-term` excludes.",
        example: Some(r#"bbox_hybrid_search(query="triad implementation", limit=10, project="/home/me/repos/erlang-test")"#),
    },
    ToolDoc {
        name: "bbox_discover_seed_entities",
        category: ToolCategory::Graph,
        summary: "Find seed entities with notable_edges; inspect before answering.",
        when_to_use: "Alternate Step 2 of the agentic opening sequence (`sm-agentic-opening-sequence`) — same blender as `bbox_hybrid_search` but with `notable_edges` rendered for each seed. Reach for it when the next step will be `bbox_inspect_entity` and you want pre-vetted hops.",
        example: Some(r#"bbox_discover_seed_entities(query="triad closure convergence test", limit=5)"#),
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
        summary: "Conversation context around a specific byte offset.",
        when_to_use: "Use after `bbox_search` or `bbox_cite` when you want the surrounding turns for a specific hit. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders.",
        example: None,
    },
    ToolDoc {
        name: "bbox_session",
        category: ToolCategory::Transcripts,
        summary: "Summary metadata for a single session.",
        when_to_use: "Use when you already have a session ID and want first prompt, project, duration, tool usage, or counts before reading the whole transcript. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders.",
        example: None,
    },
    ToolDoc {
        name: "bbox_messages",
        category: ToolCategory::Transcripts,
        summary: "Chronological messages from a session.",
        when_to_use: "Use when you need the chronological conversation flow for a known session. Supports pagination, role filter, and tail mode. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders.",
        example: None,
    },
    ToolDoc {
        name: "bbox_reindex",
        category: ToolCategory::Transcripts,
        summary: "Build or incrementally update the search index.",
        when_to_use: "Rarely — background reindexer runs every 120s. Use `full=true` after corpus corruption or schema changes.",
        example: None,
    },
    ToolDoc {
        name: "bbox_reembed",
        category: ToolCategory::Transcripts,
        summary: "Request an embedding rebuild for a configured route.",
        when_to_use: "Use after changing embedding routes or provider dimensions. E3 performs the rebuild. Routes include knowledge, code, docs, git_message, notes, threads, and guarded transcripts. Use max_entities for progressive refills. Transcript rebuilds require include_transcripts=true because they read the transcript corpus.",
        example: None,
    },
    ToolDoc {
        name: "bbox_embed_status",
        category: ToolCategory::Transcripts,
        summary: "Return per-route embedding queue health.",
        when_to_use: "Use when vector search degrades. Reports availability, queue depth, success count, and sanitized error",
        example: None,
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
        summary: "Inspect a vertex: returns properties AND targeted edges in one call. Prefer targeted inspection over broad exploration: 1) Set edge_types to the specific edges you want (e.g. 'SUPERSEDES,DERIVED_FROM'). 2) Set direction to 'out' or 'in' when you know which way to traverse. 3) Use 'both' only for initial orientation on an unfamiliar entity. 4) Set per_type_limit=0 for property-only inspection. property_mode controls detail: 'summary' (names/titles only), 'smart' (full text <=300 chars, truncated for longer - default), 'full' (no truncation).",
        when_to_use: "Step 3 of the agentic opening sequence (`sm-agentic-opening-sequence`). Prefer targeted inspection over broad sweeps. Set `edge_types` to the specific edges you want, set `direction` to `out` or `in` when known, use `both` only for initial orientation, and set `per_type_limit=0` for property-only inspection. Follow the `recommended_next_hops` list returned in the response — it is ordered semantic-first.",
        example: Some(
            r#"bbox_inspect_entity(entity_ref="knowledge:abc12345", edge_types="SUPERSEDES,DERIVED_FROM", direction="both")"#,
        ),
    },
    ToolDoc {
        name: "bbox_describe_schema",
        category: ToolCategory::Graph,
        summary: "Catalog agentic-corpus entity types, edge families, and installed agents. Use before bbox_inspect_entity, bbox_find_paths, or evidence bundling when you need the graph vocabulary, filterable fields, population counts, or traversal tips. Also use for installed-agent discovery: the agents section lists name, version, description, when_to_use, anti_patterns, cost_class, and example invocation for every active agent, grouped by dispatch_adapter.",
        when_to_use: "Step 1 of the agentic opening sequence (`sm-agentic-opening-sequence`). Use once per session for orientation; cache the schema mentally. Also use before `bbox_inspect_entity`, `bbox_find_paths`, or evidence bundling when you need graph vocabulary, edge filters, or installed-agent discovery.",
        example: Some("bbox_describe_schema()"),
    },
    ToolDoc {
        name: "bbox_find_paths",
        category: ToolCategory::Graph,
        summary: "Find direction-preserving graph paths from one EntityRef to another ref or entity type. Use after bbox_inspect_entity when a claim depends on a multi-hop chain; filter edge_types aggressively, keep max_depth small (default 3, max 5), and reuse returned path IDs with bbox_bundle_evidence. edge_types accepts a comma-separated string (e.g. 'CALLS,CALLED_BY') OR a JSON array of strings. Both shapes are equivalent.",
        when_to_use: "Step 4 of the agentic opening sequence (`sm-agentic-opening-sequence`) — only when the answer depends on a chain, not a single entity. Prefer narrow `edge_types`, set `to` or `to_type` when known, and pass returned path IDs to `bbox_bundle_evidence` before making a provenance-sensitive claim. State edge directions as the path returned them; do not invert from memory.",
        example: Some(
            r#"bbox_find_paths(from="knowledge:abc12345", edge_types="SUPERSEDES", max_depth=3)"#,
        ),
    },
    ToolDoc {
        name: "bbox_bundle_evidence",
        category: ToolCategory::Graph,
        summary: "Package selected entity refs and cached path IDs into a structured evidence bundle. Use after bbox_find_paths to close the loop before answering; stale path IDs degrade explicitly under degraded.stale_path_ids instead of failing the whole response.",
        when_to_use: "Step 5 of the agentic opening sequence (`sm-agentic-opening-sequence`) — close the loop before answering. Pass `path_ids` from `bbox_find_paths` directly; do not reconstruct path text from memory (the server holds the validated graph). This tool packages evidence only; it does not synthesize the answer for you.",
        example: Some(
            r#"bbox_bundle_evidence(question="Why was this replaced?", entity_refs=["knowledge:abc12345"], path_ids=["P1"])"#,
        ),
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
        summary: "Write bbox provenance git notes for commits with tracked tool-call anchors.",
        when_to_use: "Use after committing bbox-tracked edits when provenance should travel with git history.",
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
        summary: "Register a project directory for agentic-corpus indexing. The path must be an absolute directory path (file paths and missing paths are rejected). Re-registering the same canonical path is idempotent — returns the existing record without modifying registered_at. Triggers the project-bootstrap-arc which walks the project, chunks files, writes to the index, and emits structural edges. project_id is derived from the canonicalized realpath and is per-machine; not portable across hosts. repo_id is null for non-git projects; for git projects it derives from the first-commit SHA (with remote-URL fallback for shallow clones), so it survives clones. Use bbox_project_list to inspect registered projects.",
        when_to_use: "Use before S2+ needs a repo root. Symlink aliases collapse to one `project_id`; git repos also get `repo_id`.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_rename",
        category: ToolCategory::Projects,
        summary: "Rename a registered bbox project root while preserving its project_id and migrating project-scoped bbox state. Accepts project (project_id, registered canonical_path, or absolute path), new_path (absolute directory path), optional move_on_disk (default false), and optional dry_run. Updates project registry, knowledge, threads, notes, pins, packets, Slack channel bindings, live teams, councils, whiteboards, pollers, and crons, then reindexes project files.",
        when_to_use: "Use after renaming a repo directory, or with `move_on_disk=true` to let bbox move the directory first. Prefer `dry_run=true` before changing several project names so the affected state counts are visible.",
        example: Some(r#"bbox_project_rename(project="d723917f", new_path="/home/me/repos/blackbox", dry_run=true)"#),
    },
    ToolDoc {
        name: "bbox_project_list",
        category: ToolCategory::Projects,
        summary: "List registered project roots with their project_id, repo_id (null for non-git), canonical_path, registered_at, and is_git_repo flag. Idempotent read; safe to call repeatedly. project_ids are stable across daemon restarts. Use this before bbox_project_register to check whether a path is already registered.",
        when_to_use: "Use to inspect registered roots or confirm symlink aliases collapsed.",
        example: None,
    },
    // ── Refactor mechanization ───────────────────────────────────────
    ToolDoc {
        name: "bbox_refactor_status",
        category: ToolCategory::Refactor,
        summary: "Inspect a supported source file for tree-sitter parse health and top-level refactorable items.",
        when_to_use: "Use before structural extraction to inventory top-level items in any supported grammar, confirm tree-sitter sees the file cleanly, and copy exact item names/kinds into a language-specific bbox_refactor_plan when one exists. Pull `sm-refactor` first, then `sm-refactor-rust`, `sm-refactor-typescript`, or `sm-refactor-csharp` for language-specific arguments and validation commands.",
        example: Some(r#"bbox_refactor_status(file="src/main.rs", project_dir="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_refactor_plan",
        category: ToolCategory::Refactor,
        summary: "Create a dry-run structural refactor plan. V1 supports extract_rust_items for named top-level Rust items.",
        when_to_use: "Use to generate a reviewable plan for moving named top-level Rust items from one file to another. The plan is structural-only and includes hash checks, text edits, parse validations, selected items, and leftovers.",
        example: Some(
            r#"bbox_refactor_plan(kind="extract_rust_items", source="src/lib.rs", target="src/moved.rs", item_names=["helper"], project_dir="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bbox_refactor_apply",
        category: ToolCategory::Refactor,
        summary: "Apply a previously generated refactor plan with hash checks, Rust parse validation, atomic writes, and rollback on write failure.",
        when_to_use: "Use only after reviewing a bbox_refactor_plan result. Requires confirm=true; refuses stale file hashes and validates rewritten Rust before writing.",
        example: Some(r#"bbox_refactor_apply(plan=<plan-json>, confirm=true)"#),
    },
    // ── Knowledge ────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_learn",
        category: ToolCategory::Knowledge,
        summary: "Persist a user-stated rule or convention that should bind future sessions; rendered into provider markdown files. Use for narrative rules (\"we always X\", \"never Y\"). If the rule you're storing is actually a priority-ordered decision function, classification rubric, or structured mechanism — use `bbox_compile` instead; that produces a shareable packet any agent can apply deterministically.",
        when_to_use: "Use for standing user rules that must outlive the current edit AND would still be correct a year from now with all current arcs complete. Anti-trigger: content naming a specific migration, phase, active arc, current initiative, or \"finish X before Y\" sequencing — that's arc-bound; route to `bbox_pin`. Not for one-off task constraints, not for facts you discovered yourself (that's `bbox_note(kind=\"learned\")`). Query `bbox_knowledge` first to avoid duplicate entries. See `sm-persistence-taxonomy` via `bbox_knowledge` for the deeper split.",
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
        summary: "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces (a) rule-packets matching the query by id / domain / rule ids / classification values, and (b) system memories (code-embedded runbooks) marked `[system]`. Pass `category=\"packet\"` to list every compiled packet regardless of query. For structured packet discovery + filtering, use bbox_packet_list.",
        when_to_use: "Use near the start of tasks where durable knowledge-store context could matter: prior decisions, project conventions, rendered rules, remembered facts, or system runbooks. This is not the surface for scoped pins (`bbox_pin`), side-channel notes (`bbox_notes`/`bbox_inbox`), active threads (`bbox_thread_list`), or transcript history (`bbox_search`). Prefer a short phrase from the user's request over a single generic keyword; adjacent terms broaden recall, quoted phrases stay exact, `AND` / `OR` work explicitly, and `-term` excludes. If the first query is empty or too broad, try one sharper phrase. Use `mode=substring` for literal whole-query matching. Add `project=<cwd>` when looking for a prior decision to supersede. System memories can also be fetched by canonical `sm-*` ID. Rule-packets appear in a separate section when the query hits their id / domain / rule ids / classifications — reach for bbox_packet_list when you want structured filters (scope, latest_per_domain) or richer per-packet previews.",
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
        when_to_use: "Use to publish standing approved knowledge into managed files. `global` patches host-wide memory files; `project` writes project-local provider files that include PROJECT.md by reference. Do not use render as a way to keep active-work guidance hot across turns — that is what `bbox_pin` is for. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: Some(r#"bbox_render(scope="project", project="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_absorb",
        category: ToolCategory::Knowledge,
        summary: "Compatibility no-op for the old rendered-file import path.",
        when_to_use: "Rendered provider files are unidirectional projections now. Use `bbox_bootstrap` to import hand-authored instruction files before rendering. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
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
        when_to_use: "Use to approve or reject unverified entries, especially after `bbox_absorb`. Review state controls whether absorbed knowledge should become renderable. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: None,
    },
    ToolDoc {
        name: "bbox_bootstrap",
        category: ToolCategory::Knowledge,
        summary: "Onboard a new repo into the blackbox knowledge system.",
        when_to_use: "First-time setup for a project — seeds PROJECT.md, scaffolds knowledge structure.",
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
        summary: "Scan threads by lifecycle status and idle age.",
        when_to_use: "Before starting work on a topic (continuity check). Use `status` for lifecycle (`open`, `active`, `resolved`, `promoted`) and `min_idle_days` to return only threads idle for at least N days. Filter by `kind=work_item`.",
        example: None,
    },
    // ── Notes ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_note",
        category: ToolCategory::Notes,
        summary: "Record a structured side-channel note while working.",
        when_to_use: "Executors emit high-signal notes during work; orchestrators mostly read them. Always emit `done` before returning. Use `learned` for agent-discovered facts, not user-stated rules. See `sm-side-channel-notes` via `bbox_knowledge` for the full note taxonomy.",
        example: Some(
            r#"bbox_note(kind="dispute", body="brief assumes schema is additive — migration 0042 makes it subtractive")"#,
        ),
    },
    ToolDoc {
        name: "bbox_notes",
        category: ToolCategory::Notes,
        summary: "List / filter notes by kind, project, session, thread, resolution.",
        when_to_use: "Orchestrators reading what executors emitted this round, or auditing past dispatch for a work-item thread. Bodies are previewed at 200 chars by default; pass `full=true` to render complete bodies (useful for `done` summaries and structured `dispute` rationales).",
        example: Some(r#"bbox_notes(kind="assumption", thread_id="thread-abc", full=true)"#),
    },
    ToolDoc {
        name: "bbox_note_resolve",
        category: ToolCategory::Notes,
        summary: "Mark a note acknowledged or addressed.",
        when_to_use: "Orchestrator close-the-loop move. Pass the full `note-<8hex>` ID verbatim. `addressed` removes the note from the default inbox view; `acknowledged` keeps it visible as deferred. See `sm-side-channel-notes` via `bbox_knowledge` for the full loop.",
        example: Some(
            r#"bbox_note_resolve(id="note-a1b2c3d4", resolution="addressed", note="fixed in commit abc123")"#,
        ),
    },
    // ── Inbox ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_inbox",
        category: ToolCategory::Inbox,
        summary: "Aggregate attention layer across every store.",
        when_to_use: "Round boundaries, morning brief, any 'what needs my attention' moment. Surfaces unresolved disputes/blocked/surprises, deferred followups, stale threads, unverified knowledge, failed bro tasks. Single call, prioritized view.",
        example: Some(r#"bbox_inbox(project="/repo/x", stale_days=3)"#),
    },
    // ── Artifact catalog ─────────────────────────────────────────────
    ToolDoc {
        name: "bbox_artifact_install",
        category: ToolCategory::Artifacts,
        summary: "Install a workflow, packet, brofile, or agent artifact from a local JSON file path or http(s) URL into the versioned artifact catalog.",
        when_to_use: "Use for producer-side artifacts shipped under examples/agentic-corpus or project-local .bbox directories. The installer validates and activates the artifact through the existing workflow, packet, or brofile registry (agent artifacts receive basic JSON validation), then records version/source/supersession metadata in the catalog.",
        example: Some(
            r#"bbox_artifact_install(kind="workflow", source="examples/agentic-corpus/workflows/schema-migration-arc.json")"#,
        ),
    },
    ToolDoc {
        name: "bbox_artifact_list",
        category: ToolCategory::Artifacts,
        summary: "List installed workflow, packet, brofile, and agent artifacts with version, source, active status, and supersession metadata.",
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
        summary: "Discover compiled rule-packets before authoring a new one. Filter by `domain` (exact), `scope` (global/project), or `query` (case-insensitive substring across id, domain, rule ids, classification values). Pass `latest_per_domain=true` to collapse multiple revisions of the same domain. Each summary includes a classification histogram and the first few rule ids so you can judge relevance without calling bbox_apply. If a packet already covers your domain, compose it via `Apply{packet_id, expect}` or reuse via `bbox_apply`. See sm-rule-packets via bbox_knowledge.",
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
    // ── Orchestration (bro) ──────────────────────────────────────────
    ToolDoc {
        name: "bro_exec",
        category: ToolCategory::Orchestration,
        summary: "Launch an agent task. Returns {taskId, sessionId} immediately.",
        when_to_use: "Use to start a fresh agent session. Prefer `bro:` over raw `provider:` so routing stays stable. Follow with `bro_wait`, `bro_when_all`, or `bro_status` depending on whether you need blocking completion. See `sm-bro-dispatch-patterns` via `bbox_knowledge` for workflow shapes.",
        example: Some(
            r#"bro_exec(bro="executor", prompt="refactor the tail module", project_dir="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bro_resume",
        category: ToolCategory::Orchestration,
        summary: "Continue an existing session with a follow-up. Single-flight per provider session.",
        when_to_use: "Use for follow-ups on an existing bro session. Do not use `bro_exec` again when you need continuity. Never call `bro_resume` on a session while its previous task is still running: first `bro_wait(task_id=...)`, or `bro_cancel(task_id=...)` if you are abandoning that turn. Named bro targeting auto-resolves the session ID. See `sm-bro-dispatch-patterns` via `bbox_knowledge` for workflow shapes.",
        example: Some(
            r#"bro_resume(bro="executor", prompt="add tests for the edge case we discussed")"#,
        ),
    },
    ToolDoc {
        name: "badgey_exec",
        category: ToolCategory::Orchestration,
        summary: "Start a Badgey consultant instance for a project scope and return its badgey_id, provider session, task, and thread-of-record ids.",
        when_to_use: "Use when you want Badgey to consult over a project with continuity. The wrapper opens a work-item thread, dispatches the badgey brofile, and owns the session mapping. Use `badgey_resume` or `badgey_ask` for follow-up turns.",
        example: Some(r#"badgey_exec(project_dir="/repo/x", brief="help me navigate the agent graph work")"#),
    },
    ToolDoc {
        name: "badgey_resume",
        category: ToolCategory::Orchestration,
        summary: "Send a turn to an existing Badgey instance. Mechanical commands such as `dismiss` are handled by the wrapper before provider resume.",
        when_to_use: "Use for any follow-up where Badgey should keep its thread-of-record context. Calls are serialized per badgey_id so concurrent callers do not corrupt the provider session.",
        example: Some(r#"badgey_resume(badgey_id="bg-0123abcd-4567ef89", prompt="teach me why this edge matters")"#),
    },
    ToolDoc {
        name: "badgey_ask",
        category: ToolCategory::Orchestration,
        summary: "Question-shaped alias for badgey_resume.",
        when_to_use: "Use when the caller is asking a direct question of an existing Badgey instance and you prefer `question` over `prompt` in the request shape.",
        example: Some(r#"badgey_ask(badgey_id="bg-0123abcd-4567ef89", question="what should I inspect next?")"#),
    },
    ToolDoc {
        name: "badgey_dismiss",
        category: ToolCategory::Orchestration,
        summary: "Dismiss a Badgey instance, drain queued turns, write a dismiss event, and resolve its thread of record.",
        when_to_use: "Use when a Badgey consultation is done or should stop accepting turns. After dismissal, new resumes for that badgey_id fail with instance_dismissed.",
        example: Some(r#"badgey_dismiss(badgey_id="bg-0123abcd-4567ef89", reason="work complete")"#),
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
        example: Some(r#"badgey_scout(badgey_id="bg-0123abcd-4567ef89", charter="compare these two graph paths")"#),
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
        summary: "Block until a single task completes.",
        when_to_use: "After `bro_exec`. USE MAXIMUM TIMEOUT. Returns the final task state.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_all",
        category: ToolCategory::Orchestration,
        summary: "Block until ALL tasks / team members complete.",
        when_to_use: "Fan-out/fan-in pattern. Pair with `bro_broadcast` for blind deliberation / provider comparison. USE MAXIMUM TIMEOUT.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_any",
        category: ToolCategory::Orchestration,
        summary: "Block until the FIRST task completes.",
        when_to_use: "Racing providers / fast-path resolution. First result wins, others keep running unless cancelled.",
        example: None,
    },
    ToolDoc {
        name: "bro_broadcast",
        category: ToolCategory::Orchestration,
        summary: "Send the same prompt to every team member.",
        when_to_use: "Ensemble work. Follow with `bro_when_all` (deliberation) or `bro_when_any` (race). Resumed members are single-flight like `bro_resume`; wait or cancel a member's current task before broadcasting another turn to that same session. Interleave with individual `bro_resume` for cross-pollination between rounds.",
        example: None,
    },
    ToolDoc {
        name: "bro_status",
        category: ToolCategory::Orchestration,
        summary: "Non-blocking progress check on a task.",
        when_to_use: "Peek at a running task without blocking. Prefer `bro_wait` with a timeout when you actually need the result.",
        example: None,
    },
    ToolDoc {
        name: "bro_dashboard",
        category: ToolCategory::Orchestration,
        summary: "List recent tasks / sessions.",
        when_to_use: "Look up a taskId or sessionId when you don't already have it. Filter by provider, status, team.",
        example: None,
    },
    ToolDoc {
        name: "bro_cancel",
        category: ToolCategory::Orchestration,
        summary: "Cancel a running task (SIGTERM).",
        when_to_use: "Task is stuck, you raced another, or user asked to stop.",
        example: None,
    },
    ToolDoc {
        name: "bro_prune",
        category: ToolCategory::Orchestration,
        summary: "Drop terminal tasks from the store + persisted tasks.json.",
        when_to_use: "Stale failed/completed tasks are cluttering bro_dashboard or bbox_inbox. Defaults to status=failed. Filter by provider or older_than_hours; use dry_run=true to preview. Running tasks are never touched.",
        example: Some(r#"bro_prune(status="failed", provider="gemini")"#),
    },
    ToolDoc {
        name: "bro_providers",
        category: ToolCategory::Orchestration,
        summary: "List configured providers, binaries, models.",
        when_to_use: "Check what's available before composing a team or choosing a model.",
        example: None,
    },
    ToolDoc {
        name: "bro_brofile",
        category: ToolCategory::Orchestration,
        summary: "Manage brofile templates + accounts (provider+account+lens).",
        when_to_use: "Create, inspect, and manage reusable bro blueprints. Before `action=create`, call `action=list` first to avoid duplicates. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
        example: Some(r#"bro_brofile(action="list")"#),
    },
    ToolDoc {
        name: "bro_team",
        category: ToolCategory::Orchestration,
        summary: "Manage teamplates and instantiated teams.",
        when_to_use: "Save templates, instantiate teams, inspect roster, or tear teams down. Before `save_template` or `create`, list existing objects first to avoid duplicates. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
        example: Some(
            r#"bro_team(action="create", template="red-team", name="bbox-red", project_dir="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bro_mcp",
        category: ToolCategory::Orchestration,
        summary: "Manage MCP servers + tool filters for dispatched bros.",
        when_to_use: "Add/remove MCP servers and manage dispatch-time tool filters. Before `action=add`, call `action=list` first. The default bro-tool disallow is mechanical recursion protection, not just prose guidance. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
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
        summary: "List BadgeyProposal records owned by an instance. Returns full proposal objects (id, kind, state, draft, created_at, updated_at, events, applied_task_id) sorted by proposal_id number. Optional `since` filter (ISO timestamp) restricts to proposals created at or after that moment — useful for reading proposals emitted by the most recent Badgey turn. Used by the per-channel triage workflow's ForeachPostProposal node to iterate proposals freshly emitted by the synthesis turn.",
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
        example: Some(
            r#"badgey_ensure_for_channel(team_id="T0123ABCD", channel_id="C0123XYZ")"#,
        ),
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
        when_to_use: "Use when you want a workflow but don't want to hand-write the JSON — describe the arc shape in prose (charter), pass an authoring brofile (e.g. `probe-haiku` or a Sonnet/Opus profile for richer outputs), optionally hint at a known pattern (`crucible`, `blind-convergence`, `optimistic-review`, `linear`), and the compiler returns a validated spec. Gate/policy packet IDs come back as `packet-TODO` placeholders you fill in after compilation. Pair with `bro_orchestrate_run` for a prose-to-execution loop.",
        example: Some(
            r#"bro_orchestrate_author(charter="Review a proposal against 3 design criteria in parallel, aggregate findings, and route 'pass' or 'revise' to a final node", brofile="probe-haiku", hint="crucible")"#,
        ),
    },
    ToolDoc {
        name: "bro_orchestrate_run",
        category: ToolCategory::Workflows,
        summary: "Dispatch a workflow as a pollable task. Takes a full spec (actors, nodes with per-node `next` transitions: goto / branch / fork / terminal) and returns {taskId, arcId, status} immediately by default; poll with bro_status(task_id=...), await with bro_wait(task_id=...), or inspect arc state with bro_arc_status(arc_id=...). Pass await_completion=true only when the caller intentionally wants blocking behavior. Pass dry_run=true to validate + summarize without dispatching any bros.",
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
        summary: "Read-only structured query against active and recently-finished arcs. Returns the current ArcSnapshot (current_node, completed_nodes, in_flight_nodes, last_verdict, visit_counts, started_at) plus pending-wait registrations for the arc.",
        when_to_use: "Use to debug stuck arcs without parsing event logs — answers 'where is this arc and what's it waiting on?' in one shot. With no arc_id, lists every running arc plus all pending waits.",
        example: Some(r#"bro_arc_status(arc_id="thread-abc12345")"#),
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
        summary: "List installed webhook endpoints with their signature scheme + routing packet.",
        when_to_use: "Inventory check — what webhooks does this daemon serve? Useful before installing to avoid duplicate names.",
        example: Some("bro_webhook_list()"),
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
        summary: "List installed pollers with their schedule + source URL + routing packet.",
        when_to_use: "Inventory check before installing to avoid duplicate names; also surfaces effective tick intervals (which may be clamped above your configured value via BBOX_POLLER_MIN_INTERVAL_SECS).",
        example: Some("bro_poller_list()"),
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
        summary: "List installed crons with schedule + concurrency cap + routing packet.",
        when_to_use: "Inventory check before installing; also surfaces in-flight count so you can tell whether a cap is currently blocking a tick.",
        example: Some("bro_cron_list()"),
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
        summary: "Install a workflow spec by id so it can be referenced by name from webhook routing verdicts (`{route: start_arc, workflow: <id>}`) and other lookup paths. Compile-validated before install; capability tags enforced.",
        when_to_use: "Persist a workflow that webhooks or scheduled triggers will dispatch by name. Install alongside the routing packet that emits `start_arc` verdicts referencing this id.",
        example: Some(r#"bro_workflow_install(id="issue-to-pr", spec={...full Workflow JSON...})"#),
    },
    ToolDoc {
        name: "bro_workflow_list",
        category: ToolCategory::Workflows,
        summary: "List installed workflow specs by id.",
        when_to_use: "Inventory check — what workflows can routing verdicts target on this daemon?",
        example: Some("bro_workflow_list()"),
    },
    // ── Agents ──────────────────────────────────────────────────
    ToolDoc {
        name: "bro_agent_list",
        category: ToolCategory::Orchestration,
        summary: "List installed agents from the registry. Optional filters for cost_class, provenance_kind, include_superseded, and limit.",
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
        example: Some(r#"bro_agent_search(query="review pull request for security issues", limit=3)"#),
    },
    ToolDoc {
        name: "bro_agent_dispatch",
        category: ToolCategory::Orchestration,
        summary: "Dispatch a registered agent for a focused task. Routes through manifest dispatch_adapter if set, otherwise resolves brofile, merges filters, expands prompt template, and spawns via the standard bro execution path. Returns task_id, session, and agent attribution (agentLabel on the spawned task, preserved even when bro= routes to a named team member).",
        when_to_use: "Dispatching an agent after discovery via bro_agent_search. Returns (task_id, session) — resume with bro_resume, status with bro_status. Prefer over hand-rolling a brofile + bro_exec when the task matches an agent's description and when_to_use. Anti-pattern: do not dispatch when the agent's manifest declares one of your task's properties as an anti_pattern.",
        example: Some(r#"bro_agent_dispatch(agent="code-reviewer", args={"diff": "..."})"#),
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
        summary: "Read board state filtered for the requesting agent. Phaser-style visibility: blind phase shows only own posts; later phases reveal full board. Includes phase, phase_age_secs, ready_for_transition advisory flag, post / annotation / vote arrays scoped to what this agent should see.",
        when_to_use: "Use to inspect the board before posting / annotating / voting / transitioning. The `ready_for_transition` flag is advisory only — the facilitator still owns the actual decision. External Claudes joining mid-deliberation start here.",
        example: Some(r#"whiteboard_state(board_id="adr-2026-04-27", agent_name="security")"#),
    },
    ToolDoc {
        name: "whiteboard_annotate",
        category: ToolCategory::Whiteboards,
        summary: "Annotate a post during the validate or debate phase. Validate phase accepts only `validation` (with required `result`: confirmed / refuted / inconclusive). Debate phase accepts `challenge`, `corroborate`, or `resolve` (resolve must reference a challenge id via `resolves`).",
        when_to_use: "Use to react to other specialists' posts. You can't annotate your own post. `challenge` says you disagree (typically with reasoning), `corroborate` adds supporting evidence, `resolve` closes a challenge with a position. The challenge → resolve graph is what `ready_for_transition` checks in debate phase.",
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
        summary: "Advance the board to a new phase. Facilitator or operator role required. Sequence: blind → read → validate → debate → resolve → archived; read → debate is a legal skip. Transition emits a `board-transitioned` signal correlated to (board_id, target_phase) so any wait node observing the board resumes.",
        when_to_use: "Use to advance the deliberation when ready. Check `whiteboard_state.ready_for_transition` first as an advisory. Workflows can define a wait-on-phase node that resumes when the transition fires — this is how the engine drives multi-phase arcs through the board.",
        example: Some(
            r#"whiteboard_transition(board_id="adr-2026-04-27", agent_name="facilitator", target_phase="debate", summary="all specialists posted; advancing")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_conflicts",
        category: ToolCategory::Whiteboards,
        summary: "Auto-detect conflicts between posts on a board. Returns three kinds: `direct_overlap` (same target_file + identical target_location), `cascade_collision` (post A cascades to post B's direct target), `severity_disagreement` (same finding_ref, distinct severities). Available in any phase past blind.",
        when_to_use: "Use during read / validate / debate to surface what specialists disagree on or where their proposed actions collide. The facilitator typically reviews this before transitioning to debate so contested points get explicit annotations.",
        example: Some(
            r#"whiteboard_conflicts(board_id="adr-2026-04-27", agent_name="facilitator")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_summarize",
        category: ToolCategory::Whiteboards,
        summary: "Condensed board summary without full post bodies. Returns counts per type, vote tally per post, conflict count, unresolved-challenge count, agent status (has_posted), phase age, ready_for_transition advisory.",
        when_to_use: "Use for a quick read of board state without paying the full post-body cost. Good for inbox views, gate-packet entity inputs, and long-running observers (e.g. a polling external Claude).",
        example: Some(
            r#"whiteboard_summarize(board_id="adr-2026-04-27", agent_name="facilitator")"#,
        ),
    },
    ToolDoc {
        name: "whiteboard_archive",
        category: ToolCategory::Whiteboards,
        summary: "Archive the board. Resolve phase only. Strips active state, moves to `<store>/whiteboards/archive/<id>.json`, returns summary statistics.",
        when_to_use: "Use after the deliberation completes and any synthesis artifact (ADR markdown, PR body, etc.) has been produced. Archived boards stay readable on disk for audit but no longer count toward inbox attention.",
        example: Some(r#"whiteboard_archive(board_id="adr-2026-04-27", agent_name="facilitator")"#),
    },
    // ── Councils ──────────────────────────────────────────────────
    ToolDoc {
        name: "bro_council_list",
        category: ToolCategory::Councils,
        summary: "List active and closed councils. Optional `project` filter narrows by project_dir.",
        when_to_use: "Use to find a council before reading its transcript. A council is a multi-peer chat surface — distinct from a whiteboard, which is structured deliberation. Use councils for live conversational coordination; use whiteboards for decision artifacts.",
        example: Some(r#"bro_council_list(project="/repo/x")"#),
    },
    ToolDoc {
        name: "bro_council_open",
        category: ToolCategory::Councils,
        summary: "Read full council state: metadata, charter, posts, and current envelope status.",
        when_to_use: "Use when an external agent is directed to observe a council deliberation. Returns the full transcript plus drain-state envelopes so you can tell which bros are mid-turn and which have responded.",
        example: Some(r#"bro_council_open(id="council-7f01324e")"#),
    },
    ToolDoc {
        name: "bro_council_posts",
        category: ToolCategory::Councils,
        summary: "Paginated council transcript. `since_seq` returns posts with sequence > since_seq; `limit` caps response (default 100, max 1000).",
        when_to_use: "Use to follow a council incrementally — call with the last seen sequence to fetch only new posts. Cheaper than `bro_council_open` for long-running observation.",
        example: Some(r#"bro_council_posts(id="council-7f01324e", since_seq=42, limit=50)"#),
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
- **Executor** — does the work, emits sparse high-signal `bbox_note` entries, \
and always emits `kind=done`.

## Ambient scope block

Dispatched agents receive pre-bound IDs (`session`, `project`, `bro`, and \
sometimes `thread` / `work_item`). Use them instead of reconstructing context \
from transcript history.

## Hot-path conventions

- List before create.
- `bro_exec` starts fresh; `bro_resume` continues.
- Memory lanes: `bbox_thread` (investigation state), \
`bbox_learn`/`bbox_decide` (standing rules / commitments), \
`bbox_remember` (cold grep-able facts), `bbox_pin` (arc-bound hot context). \
The one-year test picks between rendered and pin — would it still be correct \
a year from now with current arcs done?
- Workflow vs. manual dispatch: when you're about to author (or \
re-author) a multi-phase protocol with gates, retries, or ensemble \
review — reach for `bro_orchestrate_run` with a mermaid-shaped spec \
instead of pasting a discipline-protocol into an LLM and hoping it \
won't drift. The daemon owns the state machine; the LLM is a turn. \
See `sm-workflow-orchestration` via `bbox_knowledge`.
- `bbox_learn` is for user-stated rules; `bbox_note(kind=learned)` is for \
agent-discovered facts.
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

/// Bare names of every dispatch-guarded tool. Used by provider
/// filter translators that can't accept glob patterns (Codex,
/// Gemini's policy engine) to expand the current blackbox MCP prefix's
/// `bro_*` / `bbox_refactor_*` patterns into a concrete list.
pub fn orchestration_tool_names() -> Vec<&'static str> {
    TOOL_DOCS
        .iter()
        .filter(|d| {
            d.category == ToolCategory::Orchestration || d.category == ToolCategory::Refactor
        })
        .map(|d| d.name)
        .collect()
}

/// Prefix convention for blackbox-served tools in provider tool namespaces.
/// Defaults to `mcp__blackbox__`, but follows `BLACKBOX_MCP_NAME` at runtime
/// so dev/prod daemons can coexist with distinct MCP entries.
pub fn blackbox_mcp_prefix() -> String {
    crate::util::blackbox_mcp_prefix()
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
    out.push_str("The signature failure mode: agents confidently produce training-prior answers to questions whose actual answer is stored in bbox. Avoid that on work involving repo conventions, prior decisions, active runbooks, durable user preferences, bro/orchestration behavior, or anything where durable project memory could plausibly override defaults.\n\n");
    out.push_str("Prefer a short phrase from the user's request over a single generic keyword. If the first query is empty or too broad, try one sharper phrase or escalate to `bbox_hybrid_search` (vector lane catches paraphrases). Then proceed with the opening sequence above or normal implementation work using the retrieved context.\n\n");
    out.push_str(
        "Cost of a wasted query: near zero. Cost of a confident wrong answer: the entire task.\n\n",
    );

    out.push_str("## CORE RULE: capture durable user directives\n\n");
    out.push_str("**When the user states a rule, convention, or preference meant to bind future sessions, your response MUST include a `bbox_learn` (or `bbox_remember` / `bbox_decide`) call BEFORE you wrap up the task.** Mechanical enforcement — a `.gitignore` entry, a linter config, deleted code, a removed dependency — does not replace this. It enforces the rule for the current edit; it does NOT transmit the *intent* to a future session that won't see this turn. Skipping the call means the rule silently rots and a future agent re-derives the wrong answer.\n\n");
    out.push_str("Triggers (positive and negative bind equally): \"from now on\", \"always X\", \"never X\", \"we (don't) use Y\", \"prefer Y\", \"X is banned / retired / out of scope\", \"stop using X\", \"no more X\", \"house rule\", \"standing order\", \"keep X out of\", \"X must not\".\n\n");
    out.push_str("Lane selection — once you've decided the content should persist, walk the ladder and stop at the first yes:\n\n");
    out.push_str("1. Is this investigation state tied to one debug/QC walk? → `bbox_thread`\n");
    out.push_str("2. Would the statement still be correct a year from now with all current arcs complete? → `bbox_learn` or `bbox_decide`\n");
    out.push_str("3. Is it a cold searchable fact worth grepping for later but not worth every session loading? → `bbox_remember`\n");
    out.push_str("4. Otherwise — arc-bound guidance that must stay hot for one execution lane — → `bbox_pin`\n\n");
    out.push_str("The one-year test at step 2 is the load-bearing filter. Content naming a specific migration, phase, active arc, current initiative, or \"finish X before Y\" sequencing fails it and belongs in `bbox_pin`, not `bbox_learn`. Ephemeral task constraints (\"for this fix, skip tests\", \"just for today\") don't get persisted at all.\n\n");
    out.push_str("After implementing any user directive in code/config, explicitly ask yourself: did the user just state a standing rule? If yes, emit the storage call before replying.\n\n");

    out.push_str("**Scope selection.** Default to `project` for repo-local conventions. Choose `global` only when the user's phrasing explicitly reaches beyond this repo — \"across every project\", \"on every machine\", \"in every X I write\", \"I always X as a personal rule\", \"house rule on this machine\". Technology-scoped but project-agnostic statements (\"in all Rust code I write\", \"always prefer fd over find\") are `global`. Strong wording alone is not enough — \"we always use tokio here\" stays `project`. Presence of a current project does not imply `project` scope when the user states a cross-project personal rule. If both readings are plausible, choose `project`.\n\n");

    let categories = [
        ToolCategory::Transcripts,
        ToolCategory::Graph,
        ToolCategory::Projects,
        ToolCategory::Refactor,
        ToolCategory::Knowledge,
        ToolCategory::Threads,
        ToolCategory::Notes,
        ToolCategory::Inbox,
        ToolCategory::Artifacts,
        ToolCategory::Packets,
        ToolCategory::Orchestration,
        ToolCategory::Workflows,
        ToolCategory::Whiteboards,
        ToolCategory::Councils,
    ];

    for cat in categories {
        out.push_str(&format!("## {}\n\n", cat.heading()));

        if let Some(memory_id) = deferred_system_memory(cat) {
            out.push_str(&format!(
                "On-demand runbook: `{memory_id}` via `bbox_knowledge(query=\"{memory_id}\")`.\n\n"
            ));
            continue;
        } else {
            out.push_str(cat.intro());
            out.push_str("\n\n");
            for doc in TOOL_DOCS.iter().filter(|d| d.category == cat) {
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
    // Cap at 240 bytes — long enough for one or two informative sentences
    // per tool, short enough that the rendered tool reference stays
    // skimmable. Earlier value of 12 truncated mid-word and produced
    // unreadable lines like "Hybrid BM25+ See MCP." for every entry.
    const MAX_SUMMARY_BYTES: usize = 240;
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
pub fn sync_into_knowledge(kb: &mut crate::knowledge::Knowledge) -> Result<SyncResult> {
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

    let now = crate::util::now_iso();
    let entry = KnowledgeEntry {
        id: TOOL_DOC_ENTRY_ID.to_string(),
        title: "Blackbox tool reference".to_string(),
        content,
        cluster: None,
        variants: Default::default(),
        category: Category::Tool,
        scope: Scope::Global,
        project: None,
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
    fn rendered_tool_reference_stays_prompt_sized() {
        let md = render_markdown();
        assert!(
            md.len() < 25_000,
            "rendered tool reference is too large for always-hot global memory: {} bytes",
            md.len()
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

    /// Parse `#[tool(...)]` attributes from main.rs. Tolerates:
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
        let src = include_str!("main.rs");
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
                    || n.starts_with("whiteboard_")
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
        let needle = format!("{key}");
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
            "no tools found in main.rs — parse regressed"
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
            "tools registered in main.rs without a ToolDoc stanza: {missing:?}"
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
        // `#[tool(description = ...)]` (src/main.rs) must equal the
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
