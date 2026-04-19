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
        }
    }

    fn intro(&self) -> &'static str {
        match self {
            Self::Transcripts => {
                "Search and read across every Claude Code / Codex / Gemini session the host has recorded. Reach for these when the user asks about past conversations, when you need to cite the origin of a rule, or when you need context around a prior decision."
            }
            Self::Knowledge => {
                "Durable memory with three verbs: `bbox_learn` for rendered rules/conventions, `bbox_remember` for indexed-only notes you can grep later, `bbox_decide` for commitments with required rationale. Render pipeline emits provider-specific markdown files (CLAUDE.md / AGENTS.md / GEMINI.md). Prefer `remember` when unsure — it can be promoted to `learn` later."
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
                "Compressive compilation of observations into portable theories. A rule-packet is a small axiomatic theory (rank/threshold tables + rule trees + anomalies) extracted from a larger observation set. Extract once with an LLM via `bbox_compile`; evaluate arbitrarily many times with a deterministic evaluator via `bbox_apply`. Use when you have a structured dataset (authorization matrix, retry taxonomy, state-transition table) that's likely governed by a small number of rules — the packet compresses 10-50x and generalizes to entities the sender never saw. Validate fidelity via `bbox_audit` before trusting predictions."
            }
            Self::Orchestration => {
                "Dispatch agents across providers (Claude, Codex, Copilot, Vibe, Gemini). Prefer named `bro` targeting (resolves provider + account + lens + session automatically) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again — it starts fresh with no memory). For ensembles: `bro_broadcast` + `bro_when_all` (blind deliberation) or `bro_when_any` (race)."
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
        summary: "Full-text search across all indexed transcripts.",
        when_to_use: "User asks 'when did we discuss X' / 'find the session where Y' / 'what did codex say about Z'. Filterable by account, project, role. Pass exclude_self=true to suppress the caller's own current session via tail-content heuristic (off by default).",
        example: Some(r#"bbox_search(query="redis locking", project="my-app", role="user")"#),
    },
    ToolDoc {
        name: "bbox_cite",
        category: ToolCategory::Transcripts,
        summary: "Trace a claim back to the turn that established it.",
        when_to_use: "You want provenance for a rule or preference. Defaults to role=user, returns citations oldest-first so the origin surfaces first.",
        example: Some(r#"bbox_cite(claim="never kill processes by port")"#),
    },
    ToolDoc {
        name: "bbox_context",
        category: ToolCategory::Transcripts,
        summary: "Conversation context around a specific byte offset.",
        when_to_use: "You got a hit from `bbox_search` and want the surrounding turns.",
        example: None,
    },
    ToolDoc {
        name: "bbox_session",
        category: ToolCategory::Transcripts,
        summary: "Summary metadata for a single session.",
        when_to_use: "You have a session ID (from search or dashboard) and want first prompt, project, duration, tool usage, counts.",
        example: None,
    },
    ToolDoc {
        name: "bbox_messages",
        category: ToolCategory::Transcripts,
        summary: "Chronological messages from a session.",
        when_to_use: "Read conversation flow. Supports pagination, role filter, tail mode.",
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
        when_to_use: "Finding a session by project or name without a specific query.",
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
        summary: "Persist a user-stated rule or convention that should bind future sessions; rendered into provider markdown files.",
        when_to_use: "Use for standing user rules that must outlive the current edit. Not for one-off task constraints, and not for facts you discovered yourself. Query `bbox_knowledge` first to avoid duplicate entries. See `sm-persistence-taxonomy` via `bbox_knowledge` for the deeper split.",
        example: Some(
            r#"bbox_learn(content="use rustls, not openssl", category="convention", scope="project", project="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bbox_remember",
        category: ToolCategory::Knowledge,
        summary: "Persist a fact for later recall; indexed but NOT rendered.",
        when_to_use: "Observations, decisions, context worth grepping for later but not worth every session loading. Safer default than `learn` when unsure.",
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
        name: "bbox_knowledge",
        category: ToolCategory::Knowledge,
        summary: "Query stored entries by free-text or filters. First tool call on any substantive task per the CORE RULE above. Also surfaces matching system memories (code-embedded runbooks) marked `[system]` when the query hits one.",
        when_to_use: "Start here on substantive tasks. Use one distinctive keyword by default; add `project=<cwd>` when looking for a prior decision to supersede. System memories can also be fetched by canonical `sm-*` ID.",
        example: Some(r#"bbox_knowledge(query="retry")"#),
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
        when_to_use: "After adding or changing entries. Scope `global` patches host-wide memory files; `project` writes project-local files + PROJECT.md.",
        example: Some(r#"bbox_render(scope="project", project="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_absorb",
        category: ToolCategory::Knowledge,
        summary: "Import external edits to rendered files back as unverified entries.",
        when_to_use: "User hand-edited the rendered CLAUDE.md / AGENTS.md / GEMINI.md and you want to reconcile.",
        example: None,
    },
    ToolDoc {
        name: "bbox_lint",
        category: ToolCategory::Knowledge,
        summary: "Health check for contradictions, stale entries, duplicates.",
        when_to_use: "Periodic hygiene; before large refactors of the knowledge store.",
        example: None,
    },
    ToolDoc {
        name: "bbox_review",
        category: ToolCategory::Knowledge,
        summary: "Approve or reject entries awaiting review.",
        when_to_use: "Unverified entries accumulate from absorb and agent-inferred `learn` calls. Review them before they render into global memory.",
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
        summary: "Compile a set of rules into a rule-packet (a compiled axiomatic theory with a deterministic evaluator). See `sm-rule-packets` via `bbox_knowledge` for the full workflow.",
        when_to_use: "Use when a structured observation set is governed by a smaller rule system. Rules are first-match-wins, so put anomalies before general rules. Validate with `bbox_audit` before trusting predictions.",
        example: Some(r#"bbox_compile(domain="retry-policy", rules=[{"id":"transient_retry","antecedent":{"op":"Eq","field":"nature","value":"transient"},"consequent":"RETRY"},{"id":"default_fail","antecedent":{"op":"True"},"consequent":"FAIL"}])"#),
    },
    ToolDoc {
        name: "bbox_apply",
        category: ToolCategory::Packets,
        summary: "Evaluate a rule-packet against a single entity. Returns the first matching rule's consequent + rule_id.",
        when_to_use: "Ask whether a specific (role, method, resource) / (nature, origin) / etc. is ALLOW or DENY per a packet's rules. Deterministic, no LLM — cheap at arbitrary scale. If no rule matches, returns `{match: false}` rather than guessing.",
        example: Some(r#"bbox_apply(packet_id="packet-a1b2c3d4", entity={"role":"editor","method":"POST","resource":"team"})"#),
    },
    ToolDoc {
        name: "bbox_audit",
        category: ToolCategory::Packets,
        summary: "Apply a packet to a `{entity, expected}[]` dataset; return fidelity report.",
        when_to_use: "Self-verify a packet against its training observations before trusting predictions. Also used to catch sender-side over-generalization: if fidelity < 1.0, the packet has a rule that mis-predicts observed data. Mismatches list names each failing row with its expected vs predicted consequent.",
        example: Some(r#"bbox_audit(packet_id="packet-a1b2c3d4", dataset=[{"entity":{...}, "expected":"ALLOW"}, ...])"#),
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
];

pub const WORKFLOW_NOTES: &str = "\
## Retrieval cues

If a tool stanza says `See: sm-...`, fetch that runbook on demand with \
`bbox_knowledge(query=\"sm-...\")`. Keep primitive semantics hot; pull deep \
workflow guidance only when you need it.

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
- `bbox_learn` is for user-stated standing rules; `bbox_note(kind=learned)` is \
for agent-discovered facts.
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

    out.push_str("## CORE RULE: recall first\n\n");
    out.push_str("**On any substantive task, your FIRST tool call must be `bbox_knowledge(query=<one keyword>)` to check for stored project-specific context.** Not the second call after `ls`. Not after probing the live system. First.\n\n");
    out.push_str("The signature failure mode here: agents confidently produce training-prior answers to questions whose actual answer is stored in bbox. This is not a suggestion.\n\n");
    out.push_str("Use a single distinctive keyword from the task. If empty, try a different word. Do not fall back to filesystem exploration, process probing, or training-prior inference until at least 2 distinct queries have returned empty.\n\n");
    out.push_str(
        "Cost of a wasted query: near zero. Cost of a confident wrong answer: the entire task.\n\n",
    );

    out.push_str("## CORE RULE: capture durable user directives\n\n");
    out.push_str("**When the user states a rule, convention, or preference meant to bind future sessions, your response MUST include a `bbox_learn` (or `bbox_remember` / `bbox_decide`) call BEFORE you wrap up the task.** Mechanical enforcement — a `.gitignore` entry, a linter config, deleted code, a removed dependency — does not replace this. It enforces the rule for the current edit; it does NOT transmit the *intent* to a future session that won't see this turn. Skipping the call means the rule silently rots and a future agent re-derives the wrong answer.\n\n");
    out.push_str("Triggers (positive and negative bind equally): \"from now on\", \"always X\", \"never X\", \"we (don't) use Y\", \"prefer Y\", \"X is banned / retired / out of scope\", \"stop using X\", \"no more X\", \"house rule\", \"standing order\", \"keep X out of\", \"X must not\".\n\n");
    out.push_str("Scope test before emitting: would the statement still matter after this edit is reverted or forgotten? If yes, store it. If no (\"for this fix, skip tests\", \"just for today\"), don't — that's an ephemeral task constraint, not a standing rule.\n\n");
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
    /// Returns the unescaped `value` (no escape processing needed in our
    /// current corpus).
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
            let end = after.find('"')?;
            return Some(after[..end].to_string());
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
