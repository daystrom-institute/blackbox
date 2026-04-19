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
pub const SYSTEM_MEMORIES: &[SystemMemory] = &[
    SystemMemory {
        id: "sm-rule-packets",
        title: "Rule-packets — when and how",
        tags: &[
            "packets",
            "rule-packets",
            "compile",
            "apply",
            "audit",
            "review",
            "procedure",
            "runbook",
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
        title: "Review packets — code review via rule-packets",
        tags: &[
            "packets",
            "review",
            "code-review",
            "classification",
            "lattice",
            "domain",
            "runbook",
        ],
        content: include_str!("review-packets.md"),
    },
    SystemMemory {
        id: "sm-auth-packets",
        title: "Auth packets — authorization matrices via rule-packets",
        tags: &[
            "packets",
            "auth",
            "authorization",
            "allow",
            "deny",
            "classification",
            "domain",
            "runbook",
        ],
        content: include_str!("auth-packets.md"),
    },
    SystemMemory {
        id: "sm-design-packets",
        title: "Design packets — ensemble design-iteration via rule-packets",
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
        ],
        content: include_str!("design-packets.md"),
    },
];

/// Lookup by exact ID. Accepts either canonical form (`sm-rule-packets`) or
/// bare slug (`rule-packets`) for ergonomics.
pub fn get(id: &str) -> Option<&'static SystemMemory> {
    SYSTEM_MEMORIES
        .iter()
        .find(|m| m.id == id || m.id.strip_prefix("sm-") == Some(id))
}

/// Substring search. Case-insensitive; matches on title, any tag, or body.
/// `None` query means "all memories". Returns in catalog order.
pub fn search(query: Option<&str>) -> Vec<&'static SystemMemory> {
    let needle = query.map(|q| q.to_lowercase());
    SYSTEM_MEMORIES
        .iter()
        .filter(|m| match &needle {
            None => true,
            Some(q) if q.is_empty() => true,
            Some(q) => {
                m.id.to_lowercase().contains(q)
                    || m.title.to_lowercase().contains(q)
                    || m.tags.iter().any(|t| t.to_lowercase().contains(q))
                    || m.content.to_lowercase().contains(q)
            }
        })
        .collect()
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
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
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
    fn search_case_insensitive() {
        let hits = search(Some("PACKET"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
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
