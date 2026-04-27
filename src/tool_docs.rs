//! Single source of truth for the agent-facing tool reference.
//!
//! Every `bbox_*` / `bro_*` MCP tool registered in `main.rs` must have
//! a matching stanza in `TOOL_DOCS`. A unit test enforces this.
//!
//! On daemon startup, `sync_into_knowledge` upserts a fixed-ID global
//! knowledge entry (`bb-tool-reference`) rendered from `TOOL_DOCS` +
//! `WORKFLOW_NOTES`. That entry lands in `~/.claude-shared/CLAUDE.md`
//! / `~/.codex/AGENTS.md` / `~/.gemini/GEMINI.md` on the next
//! `bbox_render` pass so every agent on every project sees a current
//! tool map.
//!
//! Adding or changing a tool = one edit here. No hand-curated drift.

use anyhow::Result;

use crate::knowledge::{Approval, Category, KnowledgeEntry, Priority, Scope, Status};

pub const TOOL_DOC_ENTRY_ID: &str = "bb-tool-reference";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Transcripts,
    Knowledge,
    Threads,
    Notes,
    Inbox,
    Packets,
    Orchestration,
    Workflows,
}

impl ToolCategory {
    fn heading(&self) -> &'static str {
        match self {
            Self::Transcripts => "Transcripts",
            Self::Knowledge => "Knowledge",
            Self::Threads => "Threads",
            Self::Notes => "Side-channel notes",
            Self::Inbox => "Attention / inbox",
            Self::Packets => "Rule-packets",
            Self::Orchestration => "Bro orchestration",
            Self::Workflows => "Workflow orchestration",
        }
    }

    fn intro(&self) -> &'static str {
        match self {
            Self::Transcripts => {
                "Search and read across every Claude Code / Codex / Gemini session the host has recorded. Reach for these when the user asks about past conversations, when you need to cite the origin of a rule, or when you need context around a prior decision."
            }
            Self::Knowledge => {
                "Memory has four lanes: `bbox_learn` for standing rendered rules, `bbox_remember` for cold indexed recall, `bbox_decide` for durable commitments with rationale, and `bbox_pin` for persisted but scope-limited ambient context on an active session/bro/thread/work-item. Render pipeline emits provider-specific markdown files (CLAUDE.md / AGENTS.md / GEMINI.md) only for the standing lanes."
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
            Self::Packets => {
                "Reusable judges compiled from examples or stated rules. If your task involves writing a priority-ordered rubric, ranking a batch against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones — compile a packet. `bbox_compile` authors the mechanism, `bbox_apply` evaluates any entity deterministically (no LLM), `bbox_audit` self-validates against known labels. Packets are portable: dispatch `packet_id` to sub-agents and every one of them produces bit-identical output. See `sm-rule-packets` via `bbox_knowledge` for the full runbook."
            }
            Self::Orchestration => {
                "Dispatch agents across providers (Claude, OpenCode, Codex, Copilot, Vibe, Gemini). Prefer named `bro` targeting (resolves provider + account + lens + session automatically) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again — it starts fresh with no memory). For ensembles: `bro_broadcast` + `bro_when_all` (blind deliberation) or `bro_when_any` (race)."
            }
            Self::Workflows => {
                "Define multi-phase agent protocols as mermaid-shaped JSON specs and dispatch them as a unit. The daemon owns the state machine; actors (executor / ensemble / advisor / user) are dispatched INTO the loop as stateless turns. Gate packets route choice nodes by verdict; retry ceilings cap back-edges; fork + `late_inject` express async steering; sub-workflows compose arcs like rule-packets compose via `Apply`; workflow-level `policy_packet` mechanizes arc-health decisions without an LLM advisor. Every run opens a `bbox_thread(kind=work_item)` with structured notes + rolling compaction anchors. Replaces long skill-prose protocols (overmind, crucible). See `sm-workflow-orchestration` via `bbox_knowledge` for the full runbook and `examples/workflows/` for the catalog."
            }
        }
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
        when_to_use: "Use when you know the topic but not the exact session. Default `smart` mode treats adjacent terms as broad recall, preserves quoted phrases, and understands `-term`; switch to `mode=fulltext` when you want raw boolean query syntax with conjunction semantics. Filter by account, project, or role as early as possible. Pass `exclude_self=true` to suppress the caller's own current session. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders and query-shaping guidance.",
        example: Some(r#"bbox_search(query="redis locking", project="my-app", role="user")"#),
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
        when_to_use: "Use to publish standing approved knowledge into managed files. `global` patches host-wide memory files; `project` writes project-local files + PROJECT.md. Do not use render as a way to keep active-work guidance hot across turns — that is what `bbox_pin` is for. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: Some(r#"bbox_render(scope="project", project="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_absorb",
        category: ToolCategory::Knowledge,
        summary: "Import external edits to rendered files back as unverified entries.",
        when_to_use: "Use when rendered memory files were edited manually and you want to import those edits back into the store for reconciliation. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
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
        when_to_use: "Orchestrators reading what executors emitted this round, or auditing past dispatch for a work-item thread.",
        example: Some(r#"bbox_notes(kind="assumption", thread_id="thread-abc")"#),
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
    // ── Rule-packets ─────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_compile",
        category: ToolCategory::Packets,
        summary: "Compile a rubric / judge / decision-function into a shareable packet. Reach here when you're writing a priority-ordered rubric, ranking proposals against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones. Symptom: you're about to paste the same rubric text into multiple sub-agent prompts — compile once and dispatch the packet_id instead. Rules are first-match-wins over a predicate AST; validate with bbox_audit before trusting. Packets compose via `Apply{packet_id, expect}` — extract `is_breaking` / `privileged_role` / etc. once, reuse across packets. Full workflow: sm-rule-packets via bbox_knowledge.",
        when_to_use: "Symptoms that mean \"compile a packet\": (1) you're coordinating multiple sub-agents and pasting the same rubric text into each prompt — compile once, dispatch `packet_id` instead, guarantees bit-identical standards; (2) you're ranking a batch of proposals/PRs/incidents against shared criteria; (3) you've got 10+ labeled examples and need a mechanism that generalizes to the 100+ unlabeled ones; (4) you're about to write Python/prose to implement a decision tree. First-match-wins so put anomalies before general rules. Always follow with `bbox_audit` to verify fidelity.",
        example: Some(r#"bbox_compile(domain="pr-triage", classification_lattice=["fail","flag","manual","pass","info"], rules=[{"id":"fail_tests","classification":"fail","antecedent":{"op":"Eq","field":"tests_pass","value":false},"consequent":"REJECT"},{"id":"flag_api_change","classification":"flag","antecedent":{"op":"Eq","field":"api_surface_changed","value":true},"consequent":"FLAG"},{"id":"pass_default","classification":"pass","emit":"fallback","antecedent":{"op":"True"},"consequent":"ACCEPT"}])"#),
    },
    ToolDoc {
        name: "bbox_apply",
        category: ToolCategory::Packets,
        summary: "Evaluate a packet against one entity — deterministic, no LLM. The receive-side of the packet workflow: a sub-agent that received packet_id from its orchestrator calls this to classify without reinterpreting the rubric. mode=\"first\" returns the first matching rule; mode=\"all\" returns every matching rule plus an aggregate verdict (for review / multi-finding shape). Cheap at arbitrary scale.",
        when_to_use: "The receive-side of the packet workflow. Use from a sub-agent that received `packet_id` from its orchestrator — no need to re-read or re-interpret the rubric, just evaluate. Also use yourself after compiling to spot-check on specific entities. If no rule matches, returns `{match: false}` rather than guessing — so missing catchalls surface immediately.",
        example: Some(r#"bbox_apply(packet_id="packet-a1b2c3d4", entity={"tests_pass":true,"api_surface_changed":true,"migration_note_present":false}, mode="all")"#),
    },
    ToolDoc {
        name: "bbox_audit",
        category: ToolCategory::Packets,
        summary: "Run a packet against a {entity, expected}[] dataset; report fidelity + mismatching rule ids. The self-verify step: a packet with fidelity < 1.0 is lying about its training data. ALWAYS call this after bbox_compile against the observations you derived the rules from — catches over-generalization, rule-ordering bugs, and field-name typos.",
        when_to_use: "ALWAYS run this after `bbox_compile` against the observations you derived the rules from. Catches (a) rules that mis-generalized beyond the anomalies, (b) ordering bugs where a general rule shadows an anomaly, (c) typos in field names. Use `mode=\"all\"` when the packet is for multi-finding review and expected outputs are rule-id sets.",
        example: Some(r#"bbox_audit(packet_id="packet-a1b2c3d4", dataset=[{"entity":{"tests_pass":false,...}, "expected":"REJECT"}, ...])"#),
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
        example: Some(r#"bbox_packet_gap(description="wanted regex matching on log messages; no StringContains-like primitive", ast_feature_requested="StringMatches")"#),
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
        summary: "Continue an existing session with a follow-up.",
        when_to_use: "Use for follow-ups on an existing bro session. Do not use `bro_exec` again when you need continuity. Named bro targeting auto-resolves the session ID. See `sm-bro-dispatch-patterns` via `bbox_knowledge` for workflow shapes.",
        example: Some(
            r#"bro_resume(bro="executor", prompt="add tests for the edge case we discussed")"#,
        ),
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
        when_to_use: "Ensemble work. Follow with `bro_when_all` (deliberation) or `bro_when_any` (race). Interleave with individual `bro_resume` for cross-pollination between rounds.",
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
    // ── Workflows ────────────────────────────────────────────────────
    ToolDoc {
        name: "bro_orchestrate_author",
        category: ToolCategory::Workflows,
        summary: "Compile a prose charter into a validated workflow spec. Dispatches an authoring LLM with the sm-workflow-orchestration runbook + a minimal reference example, parses its JSON response, cross-validates via the engine's compile step, retries once on compile failure with the error appended, and returns the validated spec — ready to pass to `bro_orchestrate_run`. Closes the authoring loop: operators describe the arc in prose, get a mermaid-shaped spec back, dispatch without hand-writing the graph.",
        when_to_use: "Use when you want a workflow but don't want to hand-write the JSON — describe the arc shape in prose (charter), pass an authoring brofile (e.g. `probe-haiku` or a Sonnet/Opus profile for richer outputs), optionally hint at a known pattern (`crucible`, `blind-convergence`, `optimistic-review`, `linear`), and the compiler returns a validated spec. Gate/policy packet IDs come back as `packet-TODO` placeholders you fill in after compilation. Pair with `bro_orchestrate_run` for a prose-to-execution loop.",
        example: Some(
            r#"bro_orchestrate_author(charter="Review a proposal against 3 design criteria in parallel, aggregate findings, and route 'pass' or 'revise' to a final node", brofile="probe-haiku", hint="crucible")"#,
        ),
    },
    ToolDoc {
        name: "bro_orchestrate_run",
        category: ToolCategory::Workflows,
        summary: "Dispatch a mermaid-shaped workflow. Takes a full workflow spec (actors, nodes, embedded stateDiagram-v2 graph) and blocks until the arc terminates. Returns the event log, per-node outputs, and the `arc_thread_id` for post-hoc audit via `bbox_notes(thread_id=...)` or `bro orchestrate status`. Pass `dry_run=true` to validate + summarize without dispatching any bros. Replaces long skill-prose protocols like overmind/crucible — the daemon owns the state machine, dispatched bros are stateless function-call turns. See `sm-workflow-orchestration` via `bbox_knowledge` and `examples/workflows/` for the shape catalog.",
        when_to_use: "Use when your task has multiple phases with verdict-based branching, retry-on-fail semantics, async steering (fork + late_inject), or reusable sub-arcs — and especially when you'd otherwise be writing dozens of lines of 'advisor MUST NOT … / protocol REQUIRES …' prose to keep a top-level LLM from drifting as it coordinates. Author the spec (or copy one from `examples/workflows/`), cross-validate via `dry_run=true`, then dispatch. Follow with `bbox_notes(thread_id=<arc_thread_id>)` or `bro orchestrate status <id>` for the audit trail. Full runbook at `sm-workflow-orchestration` via `bbox_knowledge`.",
        example: Some(
            r#"bro_orchestrate_run(workflow={...full spec...}, project_dir="/repo/x", dry_run=true)"#,
        ),
    },
    ToolDoc {
        name: "bro_arc_signal",
        category: ToolCategory::Workflows,
        summary: "Resolve a pending Wait by signal name + correlation tuple. Same dispatch path that the webhook router uses for `signal_arc` verdicts — surfaced as MCP so an operator can manually advance an arc that's blocked on an external event.",
        when_to_use: "Use to manually push an arc that's parked on a Wait node when the upstream event hasn't (or won't) arrive — e.g. testing, debugging, or rescuing an arc that missed its webhook. Empty `correlate` broadcasts to all matching waits.",
        example: Some(
            r#"bro_arc_signal(signal="pr-merged", correlate={"pr": 42})"#,
        ),
    },
    ToolDoc {
        name: "bro_arc_status",
        category: ToolCategory::Workflows,
        summary: "Read-only structured query against active and recently-finished arcs. Returns the current ArcSnapshot (current_node, completed_nodes, in_flight_nodes, last_verdict, visit_counts, started_at) plus pending-wait registrations for the arc.",
        when_to_use: "Use to debug stuck arcs without parsing event logs — answers 'where is this arc and what's it waiting on?' in one shot. With no arc_id, lists every running arc plus all pending waits.",
        example: Some(r#"bro_arc_status(arc_id="thread-abc12345")"#),
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
        name: "bro_workflow_install",
        category: ToolCategory::Workflows,
        summary: "Install a workflow spec by id so it can be referenced by name from webhook routing verdicts (`{route: start_arc, workflow: <id>}`) and other lookup paths. Compile-validated before install; capability tags enforced.",
        when_to_use: "Persist a workflow that webhooks or scheduled triggers will dispatch by name. Install alongside the routing packet that emits `start_arc` verdicts referencing this id.",
        example: Some(
            r#"bro_workflow_install(id="issue-to-pr", spec={...full Workflow JSON...})"#,
        ),
    },
    ToolDoc {
        name: "bro_workflow_list",
        category: ToolCategory::Workflows,
        summary: "List installed workflow specs by id.",
        when_to_use: "Inventory check — what workflows can routing verdicts target on this daemon?",
        example: Some("bro_workflow_list()"),
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

/// Bare names of every orchestration (`bro_*`) tool. Used by provider
/// filter translators that can't accept glob patterns (Codex,
/// Gemini's policy engine) to expand the current blackbox MCP prefix's
/// `bro_*` pattern into a concrete list.
/// concrete list.
pub fn orchestration_tool_names() -> Vec<&'static str> {
    TOOL_DOCS
        .iter()
        .filter(|d| d.category == ToolCategory::Orchestration)
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

/// Render the full tool reference as markdown. Shape: category intros
/// followed by per-tool stanzas, then workflow notes.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str(
        "Blackbox tool reference — the MCP tools this daemon exposes and when to reach for them. ",
    );
    out.push_str(
        "This entry is generated from `src/tool_docs.rs` and refreshed on every daemon restart. ",
    );
    out.push_str("Do not hand-edit.\n\n");

    out.push_str("## CORE RULE: contextual recall\n\n");
    out.push_str("**Early in tasks where durable knowledge-store context could change the answer, query `bbox_knowledge` before committing to an approach.** This is a recall check, not a ritual call for every tiny command.\n\n");
    out.push_str("Use it for prior decisions, project conventions, rendered rules, remembered facts, system runbooks, and packet discovery. It is not the surface for scoped pins (`bbox_pin`), side-channel notes (`bbox_notes` / `bbox_inbox`), active threads (`bbox_thread_list`), or transcript history (`bbox_search`).\n\n");
    out.push_str("The signature failure mode here: agents confidently produce training-prior answers to questions whose actual answer is stored in bbox. Avoid that on work involving repo conventions, prior decisions, active runbooks, durable user preferences, bro/orchestration behavior, or anything where durable project memory could plausibly override defaults.\n\n");
    out.push_str("Prefer a short phrase from the user's request over a single generic keyword. If the first query is empty or too broad, try one sharper phrase. Then proceed with filesystem exploration, process probing, or normal implementation work using the retrieved context.\n\n");
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
        ToolCategory::Knowledge,
        ToolCategory::Threads,
        ToolCategory::Notes,
        ToolCategory::Inbox,
        ToolCategory::Packets,
        ToolCategory::Orchestration,
        ToolCategory::Workflows,
    ];

    for cat in categories {
        out.push_str(&format!("## {}\n\n", cat.heading()));
        out.push_str(cat.intro());
        out.push_str("\n\n");
        for doc in TOOL_DOCS.iter().filter(|d| d.category == cat) {
            out.push_str(&format!("- **`{}`** — {}\n", doc.name, doc.summary));
            out.push_str(&format!("  _When to use:_ {}\n", doc.when_to_use));
            if let Some(ex) = doc.example {
                out.push_str(&format!("  _Example:_ `{ex}`\n"));
            }
            if let Some(hint) = system_memory_hint(doc) {
                out.push_str(&hint);
            }
        }
        out.push('\n');
    }

    out.push_str(WORKFLOW_NOTES);
    out
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
    fn render_contains_every_tool_name() {
        let md = render_markdown();
        for doc in TOOL_DOCS {
            assert!(
                md.contains(doc.name),
                "rendered markdown missing {}",
                doc.name
            );
        }
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
                if n.starts_with("bbox_") || n.starts_with("bro_") {
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
