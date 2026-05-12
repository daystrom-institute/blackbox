//! System memories — code-owned knowledge procedures.
//!
//! A system memory is a markdown runbook embedded in the daemon binary via
//! `include_str!`. It's queryable through `bbox_knowledge` alongside runtime
//! entries but never rendered into CLAUDE.md / AGENTS.md / GEMINI.md — agents
//! pull it on demand when they reach for a primitive they haven't used before.
//!
//! Contrast with the other tiers:
//! - `bbox_learn` / `bbox_decide` render into provider markdown files → every
//!   agent, every turn. Best for things every agent must always know.
//! - `bbox_remember` → runtime fact store, queryable but not rendered.
//! - `bbox_pin` → scoped ambient context for one active execution lane;
//!   persisted but not rendered.
//! - `WORKFLOW_NOTES` in `tool_docs.rs` → rendered; reserved for short always-
//!   in-context guidance.
//! - **System memories (here)** → code-embedded, not rendered, discoverable
//!   only by explicit query. Good for focused runbooks, protocol primers,
//!   workflow-specific guidance that's 500+ words.
//!
//! Lifecycle: edit the `.md` file, rebuild the daemon, restart. No DB writes,
//! no pruning, no versioning drift. The binary is the source of truth.
//!
//! Discovery: tool descriptions carry a one-line pointer. Example —
//! `bbox_compile` description mentions "Full workflow in `sm-rule-packets` —
//! query via bbox_knowledge." Agent reads tool description, spots pointer,
//! pulls the runbook with one query when they actually need it.

use crate::query::{parse_query, QueryAtom, QueryNode};

#[derive(Debug, Clone, Copy)]
pub struct SystemMemory {
    /// Canonical ID: `sm-<slug>`. Stable across daemon restarts because it's
    /// compiled in. Agents cite memories by ID when referring to procedures.
    pub id: &'static str,
    pub title: &'static str,
    /// Searchable tags. Queries match on any tag substring.
    pub tags: &'static [&'static str],
    /// Embedded markdown body via `include_str!`.
    pub content: &'static str,
}

/// The full catalog. Order is stable; agents fetching by ID are immune to it,
/// but listings return in this order so hand-curated priority is preserved.
/// `sm-agentic-opening-sequence` sits at position [0] so any unfiltered
/// listing surfaces it first — it's the cold-agent grounding pattern that
/// every other primitive composes into.
pub const SYSTEM_MEMORIES: &[SystemMemory] = &[
    SystemMemory {
        id: "sm-agentic-opening-sequence",
        title: "Agentic opening sequence — orient, search, inspect, traverse, answer",
        tags: &[
            // Direct names
            "opening",
            "opening-sequence",
            "grounding",
            "first-step",
            "first-loop",
            "agentic",
            "agentic-tools",
            "discover",
            "discover-seed",
            "inspect",
            "inspect-entity",
            "find-paths",
            "bundle-evidence",
            "bundle",
            "describe-schema",
            // Subject-facing vocabulary cold agents reach for when they
            // don't yet know the bbox surface. These are the queries that
            // SHOULD route to this runbook before the agent defaults to a
            // single bbox_knowledge call.
            "where",
            "what",
            "why",
            "who",
            "how",
            "when",
            "trace",
            "chain",
            "blast-radius",
            "impact",
            "lineage",
            "history",
            "provenance",
            "navigate",
            "search-quality",
            "graph-walk",
            "answer-protocol",
            "verification",
            "self-check",
            "answer",
        ],
        content: include_str!("agentic-opening-sequence.md"),
    },
    SystemMemory {
        id: "sm-rule-packets",
        title: "Rule-packets — compile a reusable mechanism from examples",
        tags: &[
            "packets",
            "rule-packets",
            "compile",
            "apply",
            "audit",
            "review",
            "procedure",
            "runbook",
            // Subject-facing vocabulary (phase-2 discovery-layer lever
            // per thread-f019d73a): cold agents phrase packet-worthy
            // tasks as "judge", "rubric", "mechanism", "evaluator",
            // "classifier", "decision", rather than "packet" or "compile".
            // These tags ensure bbox_knowledge queries from those angles
            // surface the runbook.
            "mechanism",
            "evaluator",
            "judge",
            "rubric",
            "classifier",
            "classify",
            "decision-function",
            "examples-to-rules",
            "reusable",
            "derive",
        ],
        content: include_str!("rule-packets.md"),
    },
    SystemMemory {
        id: "sm-persistence-taxonomy",
        title: "Persistence taxonomy — learn vs remember vs decide vs note",
        tags: &[
            "persistence",
            "taxonomy",
            "learn",
            "remember",
            "decide",
            "note",
            "memory",
            "runbook",
        ],
        content: include_str!("persistence-taxonomy.md"),
    },
    SystemMemory {
        id: "sm-scoped-pins",
        title: "Scoped pins — hot context for one active execution lane",
        tags: &[
            "pin",
            "pins",
            "scoped",
            "ambient",
            "session",
            "bro",
            "thread",
            "work_item",
            "active-arc",
            "runbook",
        ],
        content: include_str!("scoped-pins.md"),
    },
    SystemMemory {
        id: "sm-bro-dispatch-patterns",
        title: "Bro dispatch patterns — exec, resume, wait, race, deliberate",
        tags: &[
            "bro",
            "dispatch",
            "resume",
            "wait",
            "orchestration",
            "patterns",
            "runbook",
        ],
        content: include_str!("bro-dispatch-patterns.md"),
    },
    SystemMemory {
        id: "sm-create-etiquette",
        title: "Create etiquette — list before create",
        tags: &[
            "create",
            "dedupe",
            "list",
            "knowledge",
            "threads",
            "bro",
            "runbook",
        ],
        content: include_str!("create-etiquette.md"),
    },
    SystemMemory {
        id: "sm-refactor",
        title: "Refactor mechanization catalog — language routing and support matrix",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "language",
            "catalog",
            "support-matrix",
            "tree-sitter",
            "bbox_refactor_status",
            "bbox_refactor_plan",
            "bbox_refactor_apply",
            "rust",
            "typescript",
            "javascript",
            "csharp",
            "python",
            "java",
            "go",
            "c",
            "cpp",
            "sm-refactor-rust",
            "sm-refactor-typescript",
            "sm-refactor-csharp",
            "sm-refactor-python",
            "sm-refactor-java",
            "sm-refactor-java-extract-class",
            "sm-refactor-java-lombokify",
            "sm-refactor-go",
            "sm-refactor-c-cpp",
        ],
        content: include_str!("refactor.md"),
    },
    SystemMemory {
        id: "sm-refactor-rust",
        title: "Rust refactor mechanization — tree-sitter inventory and writable item extraction",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "rust",
            "rs",
            "tree-sitter",
            "bbox_refactor_status",
            "bbox_refactor_plan",
            "bbox_refactor_apply",
            "extract_rust_items",
            "cargo",
            "rust-analyzer",
            "symbol",
            "rename",
            "move",
            "extract",
        ],
        content: include_str!("refactor-rust.md"),
    },
    SystemMemory {
        id: "sm-refactor-typescript",
        title: "TypeScript and JavaScript refactor mechanization — tree-sitter inventory and validation workflow",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "typescript",
            "javascript",
            "tsx",
            "jsx",
            "tsserver",
            "tree-sitter",
            "bbox_refactor_status",
            "symbol",
            "rename",
            "move",
            "extract",
            "typecheck",
        ],
        content: include_str!("refactor-typescript.md"),
    },
    SystemMemory {
        id: "sm-refactor-csharp",
        title: "C# refactor mechanization — tree-sitter inventory and Roslyn validation workflow",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "csharp",
            "c#",
            "cs",
            "roslyn",
            "omnisharp",
            "tree-sitter",
            "bbox_refactor_status",
            "symbol",
            "rename",
            "move",
            "extract",
            "dotnet",
        ],
        content: include_str!("refactor-csharp.md"),
    },
    SystemMemory {
        id: "sm-refactor-python",
        title: "Python refactor mechanization — tree-sitter inventory and Pyright/Rope validation workflow",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "python",
            "py",
            "pyright",
            "jedi",
            "rope",
            "ruff",
            "pytest",
            "tree-sitter",
            "bbox_refactor_status",
            "symbol",
            "rename",
            "move",
            "extract",
        ],
        content: include_str!("refactor-python.md"),
    },
    SystemMemory {
        id: "sm-refactor-java",
        title: "Java refactor mechanization — tree-sitter inventory and JDT validation workflow",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "java",
            "jdt",
            "jdtls",
            "intellij",
            "eclipse",
            "maven",
            "gradle",
            "tree-sitter",
            "bbox_refactor_status",
            "bbox_refactor_plan",
            "bbox_refactor_apply",
            "bbox_refactor_run",
            "symbol",
            "rename",
            "move",
            "extract",
            "extract_java_methods",
            "extract_java_class",
            "extract_java_nested_classes",
            "promote_java_inner_class",
            "extract_java_interface",
            "add_java_implements",
            "migrate_java_type_usages",
            "java_lsp_organize_imports",
            "rewrite_java_visibility",
            "find_java_usages",
            "lombokify_java_class",
            "sm-refactor-java-extract-class",
            "sm-refactor-java-lombokify",
        ],
        content: include_str!("refactor-java.md"),
    },
    SystemMemory {
        id: "sm-refactor-java-extract-class",
        title: "Java extract_java_class — composite class extraction, capture analysis, FIXME catalog",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "java",
            "extract",
            "extract_java_class",
            "extract_java_methods",
            "move_java_fields",
            "move_java_constant",
            "add_java_fields",
            "add_java_constructor",
            "add_java_delegate_field",
            "rewrite_java_calls_to_delegate",
            "tree-sitter",
            "bbox_refactor_plan",
            "bbox_refactor_apply",
            "capture-analysis",
            "captured_variables",
            "callback_externals",
            "wiring_mode",
            "source_delegate_wrappers",
            "propagate_class_annotations",
            "rewrite_remaining_accessors",
            "deep_analysis",
            "external_calls",
            "inherited_dependencies",
            "remaining_source_accessors",
            "remaining_source_constant_refs",
            "FIXME",
            "delegate",
            "validation_failed",
            "mutable_capture_with_write",
            "method_overload_ambiguous",
            "guice_field_inject",
        ],
        content: include_str!("refactor-java-extract-class.md"),
    },
    SystemMemory {
        id: "sm-refactor-java-lombokify",
        title: "Java lombokify_java_class — hand-rolled POJO boilerplate to Lombok annotations",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "java",
            "lombok",
            "lombokify",
            "lombokify_java_class",
            "POJO",
            "tree-sitter",
            "bbox_refactor_plan",
            "bbox_refactor_apply",
            "bbox_refactor_run",
            "@Data",
            "@Value",
            "@Getter",
            "@Setter",
            "@EqualsAndHashCode",
            "@ToString",
            "@Slf4j",
            "@NoArgsConstructor",
            "@AllArgsConstructor",
            "@RequiredArgsConstructor",
            "boolean_getter_strategy",
            "ToStringBuilder",
            "EqualsBuilder",
            "HashCodeBuilder",
            "bulk_mode",
            "output_path",
            "annotation-processor",
        ],
        content: include_str!("refactor-java-lombokify.md"),
    },
    SystemMemory {
        id: "sm-refactor-go",
        title: "Go refactor mechanization — tree-sitter inventory and gopls validation workflow",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "go",
            "golang",
            "gopls",
            "gofmt",
            "goimports",
            "go-test",
            "tree-sitter",
            "bbox_refactor_status",
            "symbol",
            "rename",
            "move",
            "extract",
        ],
        content: include_str!("refactor-go.md"),
    },
    SystemMemory {
        id: "sm-refactor-c-cpp",
        title: "C and C++ refactor mechanization — tree-sitter inventory and clang validation workflow",
        tags: &[
            "refactor",
            "refactoring",
            "mechanization",
            "restructure",
            "c",
            "cpp",
            "c++",
            "clangd",
            "clang-rename",
            "clang-tidy",
            "clang-format",
            "cmake",
            "ninja",
            "tree-sitter",
            "bbox_refactor_status",
            "symbol",
            "rename",
            "move",
            "extract",
        ],
        content: include_str!("refactor-c-cpp.md"),
    },
    SystemMemory {
        id: "sm-side-channel-notes",
        title: "Side-channel notes — what to emit and why",
        tags: &[
            "notes",
            "bbox_note",
            "orchestrator",
            "executor",
            "done",
            "workflow",
            "runbook",
        ],
        content: include_str!("side-channel-notes.md"),
    },
    SystemMemory {
        id: "sm-transcript-retrieval",
        title: "Transcript retrieval — search, cite, context, session, messages",
        tags: &[
            "transcripts",
            "search",
            "cite",
            "context",
            "session",
            "messages",
            "retrieval",
            "runbook",
        ],
        content: include_str!("transcript-retrieval.md"),
    },
    SystemMemory {
        id: "sm-render-lifecycle",
        title: "Render lifecycle — render, absorb, review, lint",
        tags: &[
            "render",
            "absorb",
            "review",
            "lint",
            "knowledge",
            "lifecycle",
            "runbook",
        ],
        content: include_str!("render-lifecycle.md"),
    },
    SystemMemory {
        id: "sm-review-packets",
        title: "Review packets — reusable judge from example PRs/changes",
        tags: &[
            "packets",
            "review",
            "code-review",
            "classification",
            "lattice",
            "domain",
            "runbook",
            // Discovery vocabulary — subjects query with these:
            "pr-review",
            "pr-triage",
            "judge",
            "rubric",
            "mechanism",
            "evaluator",
            "triage",
            "accept-reject-flag",
            "change-quality",
            "decision",
        ],
        content: include_str!("review-packets.md"),
    },
    SystemMemory {
        id: "sm-auth-packets",
        title: "Auth packets — compress an access table into a reusable policy",
        tags: &[
            "packets",
            "auth",
            "authorization",
            "allow",
            "deny",
            "classification",
            "domain",
            "runbook",
            // Discovery vocabulary:
            "access-control",
            "access-table",
            "permissions",
            "rbac",
            "policy",
            "role",
            "resource",
            "mechanism",
            "decide-at-request-time",
            "compress-matrix",
        ],
        content: include_str!("auth-packets.md"),
    },
    SystemMemory {
        id: "sm-design-packets",
        title: "Design packets — rank proposals against shared criteria",
        tags: &[
            "packets",
            "design",
            "iteration",
            "ensemble",
            "proposals",
            "blocker",
            "concern",
            "classification",
            "domain",
            "runbook",
            // Discovery vocabulary:
            "rank-proposals",
            "evaluate-proposals",
            "compare-proposals",
            "score",
            "rubric",
            "evaluator",
            "tradeoff",
            "which-should-we-pick",
            "decision",
            "mechanism",
        ],
        content: include_str!("design-packets.md"),
    },
    SystemMemory {
        id: "sm-workflow-orchestration",
        title: "Workflow orchestration — mermaid-shaped arcs dispatched by the daemon",
        tags: &[
            "workflow",
            "workflows",
            "orchestrate",
            "orchestration",
            "arc",
            "arcs",
            "mermaid",
            "state-diagram",
            "runbook",
            // Subject-facing vocabulary — cold agents will phrase
            // workflow-shaped tasks as "protocol", "state machine",
            // "phase", "steps", "pipeline", "multi-phase",
            // "sequencer", "choreography", "process", "loop". These
            // tags ensure bbox_knowledge queries from those angles
            // surface the runbook.
            "protocol",
            "state-machine",
            "phase",
            "phases",
            "multi-phase",
            "pipeline",
            "sequencer",
            "choreography",
            "process",
            "loop",
            "crucible",
            "overmind",
            "gate",
            "gates",
            "choice",
            "fork",
            "late-inject",
            "late_inject",
            "fire-and-forget",
            "subworkflow",
            "compose",
            "composition",
            "arc-thread",
            "compaction-anchor",
            "policy-packet",
            "advisor-as-packet",
            "branch",
            "branching",
            "retry",
            "retry-loop",
            "back-edge",
            "back-edges",
            "dispatch-workflow",
            "bro-orchestrate",
            "bro_orchestrate",
        ],
        content: include_str!("workflow-orchestration.md"),
    },
    SystemMemory {
        id: "sm-whiteboards",
        title: "Whiteboards — multi-agent deliberation boards",
        tags: &[
            "whiteboard",
            "whiteboards",
            "deliberation",
            "multi-agent",
            "phase",
            "phases",
            "blind",
            "validate",
            "debate",
            "resolve",
            "facilitator",
            "claims",
            "concerns",
            "votes",
            "annotations",
            "workflow",
            "runbook",
        ],
        content: include_str!("whiteboards.md"),
    },
];

/// Lookup by exact ID. Accepts either canonical form (`sm-rule-packets`) or
/// bare slug (`rule-packets`) for ergonomics.
pub fn get(id: &str) -> Option<&'static SystemMemory> {
    SYSTEM_MEMORIES
        .iter()
        .find(|m| m.id == id || m.id.strip_prefix("sm-") == Some(id))
}

fn normalize_exact_query(raw: &str) -> &str {
    let trimmed = raw.trim();
    let unquoted = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    unquoted.trim()
}

/// Lookup by exact canonical query. This is intentionally narrower than `get`:
/// bare slugs such as `refactor` remain searchable terms, while canonical
/// `sm-refactor` fetches exactly that memory.
pub fn exact_query(query: Option<&str>) -> Option<&'static SystemMemory> {
    let candidate = normalize_exact_query(query?);
    if !candidate
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sm-"))
    {
        return None;
    }
    SYSTEM_MEMORIES
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(candidate))
}

#[derive(Debug, Clone)]
struct MemoryCorpus {
    id: String,
    title: String,
    tags: Vec<String>,
    content: String,
}

fn memory_atom_score(atom: &QueryAtom, corpus: &MemoryCorpus) -> f64 {
    let mut score = 0.0;
    if corpus.id.contains(&atom.text) {
        score += if atom.phrase { 80.0 } else { 65.0 };
    }
    if corpus.title.contains(&atom.text) {
        score += if atom.phrase { 40.0 } else { 24.0 };
    }
    if corpus.tags.iter().any(|tag| tag.contains(&atom.text)) {
        score += if atom.phrase { 32.0 } else { 20.0 };
    }
    if corpus.content.contains(&atom.text) {
        score += if atom.phrase { 12.0 } else { 6.0 };
    }
    score
}

fn memory_matches(node: &QueryNode, corpus: &MemoryCorpus) -> bool {
    match node {
        QueryNode::Atom(atom) => memory_atom_score(atom, corpus) > 0.0,
        QueryNode::And(lhs, rhs) => memory_matches(lhs, corpus) && memory_matches(rhs, corpus),
        QueryNode::Or(lhs, rhs) => memory_matches(lhs, corpus) || memory_matches(rhs, corpus),
        QueryNode::Not(inner) => !memory_matches(inner, corpus),
    }
}

fn memory_collect_score(node: &QueryNode, corpus: &MemoryCorpus) -> f64 {
    match node {
        QueryNode::Atom(atom) => memory_atom_score(atom, corpus),
        QueryNode::And(lhs, rhs) | QueryNode::Or(lhs, rhs) => {
            memory_collect_score(lhs, corpus) + memory_collect_score(rhs, corpus)
        }
        QueryNode::Not(_) => 0.0,
    }
}

/// Smart case-insensitive search. Adjacent terms broaden recall via `OR`;
/// quoted phrases, explicit `AND`/`OR`, and unary exclusion (`-term`) are
/// supported. `None` or empty query returns all memories. Results are scored
/// by where the query matched (id/title/tags/content) and returned best-first.
pub fn search(query: Option<&str>) -> Vec<&'static SystemMemory> {
    let Some(raw_query) = query.map(str::trim) else {
        return SYSTEM_MEMORIES.iter().collect();
    };
    if raw_query.is_empty() {
        return SYSTEM_MEMORIES.iter().collect();
    }
    if let Some(memory) = exact_query(Some(raw_query)) {
        return vec![memory];
    }
    let Some(ast) = parse_query(raw_query) else {
        return SYSTEM_MEMORIES.iter().collect();
    };

    let mut scored: Vec<(&'static SystemMemory, f64, usize)> = SYSTEM_MEMORIES
        .iter()
        .enumerate()
        .filter_map(|(idx, m)| {
            let corpus = MemoryCorpus {
                id: m.id.to_lowercase(),
                title: m.title.to_lowercase(),
                tags: m.tags.iter().map(|t| t.to_lowercase()).collect(),
                content: m.content.to_lowercase(),
            };
            if !memory_matches(&ast, &corpus) {
                return None;
            }
            Some((m, memory_collect_score(&ast, &corpus), idx))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
    });
    scored.into_iter().map(|(m, _, _)| m).collect()
}

/// Render one memory for an agent response: `[system] sm-…` header + title +
/// a body preview. Full body is always included — these are <2k tokens each
/// by design, and agents that matched the query want the full procedure.
pub fn format_for_listing(m: &SystemMemory) -> String {
    let mut out = String::new();
    out.push_str(&format!("[system] {} — {}\n", m.id, m.title));
    if !m.tags.is_empty() {
        out.push_str(&format!("  tags: {}\n", m.tags.join(", ")));
    }
    out.push_str("  ─────\n");
    for line in m.content.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_packets_memory_embedded_and_nonempty() {
        let m = get("sm-rule-packets").expect("sm-rule-packets must exist");
        assert!(
            m.content.len() > 500,
            "embedded rule-packets.md too short (got {} bytes)",
            m.content.len()
        );
        assert!(
            m.content.contains("bbox_compile"),
            "runbook must cite bbox_compile"
        );
        assert!(
            m.content.contains("bbox_apply"),
            "runbook must cite bbox_apply"
        );
        assert!(
            m.content.contains("bbox_audit"),
            "runbook must cite bbox_audit"
        );
    }

    #[test]
    fn search_finds_by_tag_query() {
        let hits = search(Some("packet"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_finds_by_id_query() {
        let hits = search(Some("sm-rule-packets"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sm-rule-packets");
    }

    #[test]
    fn search_exact_canonical_id_does_not_expand_prefix_family() {
        let hits = search(Some("sm-refactor"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sm-refactor");

        let quoted = search(Some("\"sm-refactor\""));
        assert_eq!(quoted.len(), 1);
        assert_eq!(quoted[0].id, "sm-refactor");
    }

    #[test]
    fn search_bare_slug_still_behaves_as_search_term() {
        let hits = search(Some("refactor"));
        assert!(hits.iter().any(|m| m.id == "sm-refactor"));
        assert!(hits.iter().any(|m| m.id == "sm-refactor-rust"));
    }

    #[test]
    fn search_finds_by_title_query() {
        let hits = search(Some("rule-packets"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_finds_by_body_content() {
        // Universal-mechanism runbook mentions "generating function".
        let hits = search(Some("generating function"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
        // Review-domain runbook mentions "adversarial".
        let review_hits = search(Some("adversarial"));
        assert!(review_hits.iter().any(|m| m.id == "sm-review-packets"));
    }

    #[test]
    fn search_finds_refactor_language_memories() {
        let catalog_hits = search(Some("refactor support matrix"));
        assert!(catalog_hits.iter().any(|m| m.id == "sm-refactor"));

        let rust_hits = search(Some("rust refactor extract_rust_items"));
        assert!(rust_hits.iter().any(|m| m.id == "sm-refactor-rust"));

        let ts_hits = search(Some("typescript refactor tsserver"));
        assert!(ts_hits.iter().any(|m| m.id == "sm-refactor-typescript"));

        let csharp_hits = search(Some("csharp refactor roslyn"));
        assert!(csharp_hits.iter().any(|m| m.id == "sm-refactor-csharp"));

        let python_hits = search(Some("python refactor pyright"));
        assert!(python_hits.iter().any(|m| m.id == "sm-refactor-python"));

        let java_hits = search(Some("java refactor jdt"));
        assert!(java_hits.iter().any(|m| m.id == "sm-refactor-java"));

        let go_hits = search(Some("go refactor gopls"));
        assert!(go_hits.iter().any(|m| m.id == "sm-refactor-go"));

        let cpp_hits = search(Some("cpp refactor clangd"));
        assert!(cpp_hits.iter().any(|m| m.id == "sm-refactor-c-cpp"));
    }

    #[test]
    fn search_case_insensitive() {
        let hits = search(Some("PACKET"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_defaults_adjacent_terms_to_or() {
        let hits = search(Some("adversarial rubric"));
        assert!(hits.iter().any(|m| m.id == "sm-review-packets"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_honors_and_and_exclusion() {
        let hits = search(Some("packets AND review -access-table"));
        assert!(hits.iter().any(|m| m.id == "sm-review-packets"));
        assert!(!hits.iter().any(|m| m.id == "sm-auth-packets"));
    }

    #[test]
    fn search_empty_returns_all() {
        let all = search(None);
        assert_eq!(all.len(), SYSTEM_MEMORIES.len());
        let empty = search(Some(""));
        assert_eq!(empty.len(), SYSTEM_MEMORIES.len());
    }

    #[test]
    fn get_accepts_canonical_and_bare() {
        assert!(get("sm-rule-packets").is_some());
        assert!(get("rule-packets").is_some());
        assert!(get("nonexistent").is_none());
    }

    #[test]
    fn format_for_listing_has_system_prefix() {
        let m = get("sm-rule-packets").unwrap();
        let out = format_for_listing(m);
        assert!(out.starts_with("[system] sm-rule-packets"));
        assert!(out.contains("Rule-packets"));
    }

    #[test]
    fn no_duplicate_ids() {
        let mut ids: Vec<_> = SYSTEM_MEMORIES.iter().map(|m| m.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate ids in SYSTEM_MEMORIES");
    }

    #[test]
    fn all_ids_canonical_prefix() {
        for m in SYSTEM_MEMORIES {
            assert!(
                m.id.starts_with("sm-"),
                "id must use canonical `sm-<slug>` form: {}",
                m.id
            );
        }
    }
}
