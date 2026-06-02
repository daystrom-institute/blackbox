//! System memory catalog and in-memory search primitives.

use crate::query::{QueryAtom, QueryNode, parse_query};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;
use tracing::info;

#[derive(Debug, Clone)]
pub struct SystemMemory {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub content: String,
}

#[derive(Debug)]
pub struct MemoryCatalog {
    memories: Vec<SystemMemory>,
}

#[derive(Debug, Clone)]
struct MemoryEntry {
    id: String,
    title: String,
    tags: Vec<String>,
    content: String,
    order: usize,
}

impl MemoryCatalog {
    pub fn load(defaults_dir: &Path, user_dir: Option<&Path>, ctx: &Value) -> Result<Self> {
        let defaults = crate::system_memory::loader::load_dir(defaults_dir)
            .with_context(|| format!("failed to load defaults from {}", defaults_dir.display()))?;

        if defaults.is_empty() {
            anyhow::bail!(
                "system memory defaults directory is empty or missing: {}",
                defaults_dir.display()
            );
        }

        let mut entries = HashMap::new();
        let mut seen_defaults = HashSet::new();

        for raw in defaults {
            if !seen_defaults.insert(raw.slug.clone()) {
                anyhow::bail!("duplicate memory slug '{}' in defaults directory", raw.slug);
            }

            let slug = raw.slug.clone();
            let content = render_body(&raw, ctx)?;
            entries.insert(
                slug.clone(),
                MemoryEntry {
                    id: format!("sm-{slug}"),
                    title: raw.front_matter.title,
                    tags: raw.front_matter.tags,
                    content,
                    order: raw.front_matter.order,
                },
            );
        }

        if let Some(user_dir) = user_dir {
            let user_memories =
                crate::system_memory::loader::load_dir(user_dir).with_context(|| {
                    format!("failed to load user memories from {}", user_dir.display())
                })?;
            let mut seen_user = HashSet::new();

            for raw in user_memories {
                if !seen_user.insert(raw.slug.clone()) {
                    anyhow::bail!("duplicate memory slug '{}' in user directory", raw.slug);
                }

                let id = format!("sm-{}", raw.slug);
                if entries.contains_key(&raw.slug) {
                    info!(memory = %id, path = %user_dir.display(), "overriding default system memory");
                }

                let slug = raw.slug.clone();
                let content = render_body(&raw, ctx)?;
                entries.insert(
                    slug,
                    MemoryEntry {
                        id,
                        title: raw.front_matter.title,
                        tags: raw.front_matter.tags,
                        content,
                        order: raw.front_matter.order,
                    },
                );
            }
        }

        let mut ordered: Vec<_> = entries.into_values().collect();
        ordered.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));

        let memories = ordered
            .into_iter()
            .map(|entry| SystemMemory {
                id: entry.id,
                title: entry.title,
                tags: entry.tags,
                content: entry.content,
            })
            .collect();

        Ok(Self { memories })
    }

    pub fn get(&self, id: &str) -> Option<&SystemMemory> {
        self.memories
            .iter()
            .find(|memory| memory.id == id || memory.id.strip_prefix("sm-") == Some(id))
    }

    pub fn exact_query(&self, query: Option<&str>) -> Option<&SystemMemory> {
        let candidate = normalize_exact_query(query?);
        if !candidate.starts_with("sm-") {
            return None;
        }

        self.memories
            .iter()
            .find(|memory| memory.id.eq_ignore_ascii_case(candidate))
    }

    pub fn search(&self, query: Option<&str>) -> Vec<&SystemMemory> {
        let Some(raw_query) = query.map(str::trim) else {
            return self.memories.iter().collect();
        };
        if raw_query.is_empty() {
            return self.memories.iter().collect();
        }
        if let Some(memory) = self.exact_query(Some(raw_query)) {
            return vec![memory];
        }
        let Some(ast) = parse_query(raw_query) else {
            return self.memories.iter().collect();
        };

        let mut scored: Vec<(&SystemMemory, f64, usize)> = self
            .memories
            .iter()
            .enumerate()
            .filter_map(|(idx, memory)| {
                let corpus = MemoryCorpus {
                    id: memory.id.to_lowercase(),
                    title: memory.title.to_lowercase(),
                    tags: memory.tags.iter().map(|tag| tag.to_lowercase()).collect(),
                    content: memory.content.to_lowercase(),
                };
                if !memory_matches(&ast, &corpus) {
                    return None;
                }
                Some((memory, memory_collect_score(&ast, &corpus), idx))
            })
            .collect();

        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.cmp(&b.2))
        });

        scored.into_iter().map(|(memory, _, _)| memory).collect()
    }

    pub fn format_catalog_summary(&self, query: Option<&str>) -> String {
        let memories = self.search(query);
        let mut out = format!(
            "── System memories ({}) ──────────────────────\n",
            memories.len()
        );
        for memory in memories {
            out.push_str(&format!("[system] {} — {}\n", memory.id, memory.title));
            if !memory.tags.is_empty() {
                let preview = memory
                    .tags
                    .iter()
                    .take(12)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("  tags: {preview}\n"));
            }
        }
        out
    }
}

fn render_body(raw: &crate::system_memory::loader::RawMemory, ctx: &Value) -> Result<String> {
    if raw.front_matter.template {
        crate::template::render(&raw.body, ctx)
            .with_context(|| format!("rendering system memory template {}", raw.slug))
    } else {
        Ok(raw.body.clone())
    }
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

pub fn format_for_listing(memory: &SystemMemory) -> String {
    let mut out = String::new();
    out.push_str(&format!("[system] {} — {}\n", memory.id, memory.title));
    if !memory.tags.is_empty() {
        out.push_str(&format!("  tags: {}\n", memory.tags.join(", ")));
    }
    out.push_str("  ─────\n");
    for line in memory.content.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Max bytes of the one-line content preview shown in a signpost.
const SIGNPOST_PREVIEW_BYTES: usize = 160;

/// Render one memory as a compact signpost: id + title, tag line, a one-line
/// content preview, and a retrieval breadcrumb pointing at the qualified-id
/// query that returns the full runbook body.
///
/// This is the renderer for the *broad* `bbox_knowledge` surface path: a fuzzy
/// multi-term query can match many runbooks, and dumping every full body
/// overflows the token budget (system memory bodies reach ~40KB each). A
/// signpost stays a few hundred bytes and tells the agent exactly how to pull
/// the body it wants — `bbox_knowledge(query="sm-<id>")` short-circuits to the
/// full body via the exact-id path.
pub fn format_for_signpost(memory: &SystemMemory) -> String {
    let mut out = String::new();
    out.push_str(&format!("[system] {} — {}\n", memory.id, memory.title));
    if !memory.tags.is_empty() {
        out.push_str(&format!("  tags: {}\n", memory.tags.join(", ")));
    }
    if let Some(preview) = signpost_preview(&memory.content) {
        out.push_str(&format!("  {preview}\n"));
    }
    out.push_str(&format!(
        "  → full runbook: bbox_knowledge(query=\"{}\")\n",
        memory.id
    ));
    out
}

/// Derive a one-line content preview: the first prose line (skipping blank
/// lines and markdown heading/list markers), truncated at a UTF-8 boundary.
fn signpost_preview(content: &str) -> Option<String> {
    let line = content.lines().map(str::trim).find(|line| {
        !line.is_empty() && !line.chars().all(|c| matches!(c, '#' | '-' | '=' | '*' | '─' | ' '))
    })?;
    let line = line.trim_start_matches(['#', '-', '*', '>', ' ']).trim();
    if line.is_empty() {
        return None;
    }
    if line.len() <= SIGNPOST_PREVIEW_BYTES {
        return Some(line.to_string());
    }
    let mut cut = SIGNPOST_PREVIEW_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}…", &line[..cut]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn fixture_default_catalog() -> MemoryCatalog {
        let defaults = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("system-defaults")
            .join("memories");
        MemoryCatalog::load(&defaults, None, &serde_json::json!({})).unwrap()
    }

    #[test]
    fn load_and_keep_default_order() {
        let catalog = fixture_default_catalog();
        // 29 .md files on disk minus the `system-memory-catalog.md` nav-map
        // (explicitly ignored by the loader, see loader.rs IGNORED_FILES) = 28.
        // Was 28 until Phase 6 (a5906c9f) deleted refactor-java-lombokify.md
        // when lombok was dissolved (→ 27); macros.md (sm-macros) added it back
        // to 28.
        assert_eq!(catalog.memories.len(), 28);
        assert_eq!(catalog.memories[0].id, "sm-agentic-opening-sequence");
        assert_eq!(catalog.memories[1].id, "sm-atoms");
    }

    #[test]
    fn load_overlay_replaces_default() {
        let default_dir = tempdir().unwrap();
        fs::write(
            default_dir.path().join("gap-notes.md"),
            "+++\ntitle = \"Default\"\ntags = [\"default\"]\norder = 1\n+++\n\ndefault body\n",
        )
        .unwrap();

        let user_dir = tempdir().unwrap();
        fs::write(
            user_dir.path().join("gap-notes.md"),
            "+++\ntitle = \"User\"\ntags = [\"user\"]\norder = 1\n+++\n\nuser body\n",
        )
        .unwrap();

        let catalog = MemoryCatalog::load(
            default_dir.path(),
            Some(user_dir.path()),
            &serde_json::json!({}),
        )
        .unwrap();

        let memory = catalog.get("sm-gap-notes").unwrap();
        assert_eq!(memory.title, "User");
        assert_eq!(memory.tags, vec!["user"]);
        assert_eq!(memory.content, "user body\n");
    }

    #[test]
    fn load_overlay_can_add_new_memory() {
        let default_dir = tempdir().unwrap();
        fs::write(
            default_dir.path().join("base.md"),
            "+++\ntitle = \"Base\"\ntags = [\"base\"]\norder = 1\n+++\n\nbase body\n",
        )
        .unwrap();

        let user_dir = tempdir().unwrap();
        fs::write(
            user_dir.path().join("local.md"),
            "+++\ntitle = \"Local\"\ntags = [\"local\"]\norder = 2\n+++\n\nlocal body\n",
        )
        .unwrap();

        let catalog = MemoryCatalog::load(
            default_dir.path(),
            Some(user_dir.path()),
            &serde_json::json!({}),
        )
        .unwrap();

        assert!(catalog.get("sm-base").is_some());
        let memory = catalog.get("sm-local").unwrap();
        assert_eq!(memory.title, "Local");
        assert_eq!(memory.content, "local body\n");
    }

    #[test]
    fn load_overlay_empty_dir_keeps_defaults() {
        let default_dir = tempdir().unwrap();
        fs::write(
            default_dir.path().join("base.md"),
            "+++\ntitle = \"Base\"\ntags = [\"base\"]\norder = 1\n+++\n\nbase body\n",
        )
        .unwrap();

        let user_dir = tempdir().unwrap();
        let catalog = MemoryCatalog::load(
            default_dir.path(),
            Some(user_dir.path()),
            &serde_json::json!({}),
        )
        .unwrap();

        assert_eq!(catalog.search(None).len(), 1);
        assert_eq!(catalog.get("sm-base").unwrap().content, "base body\n");
    }

    #[test]
    fn load_overlay_missing_dir_keeps_defaults() {
        let default_dir = tempdir().unwrap();
        fs::write(
            default_dir.path().join("base.md"),
            "+++\ntitle = \"Base\"\ntags = [\"base\"]\norder = 1\n+++\n\nbase body\n",
        )
        .unwrap();

        let catalog = MemoryCatalog::load(
            default_dir.path(),
            Some(&default_dir.path().join("missing-user-dir")),
            &serde_json::json!({}),
        )
        .unwrap();

        assert_eq!(catalog.search(None).len(), 1);
        assert_eq!(catalog.get("sm-base").unwrap().content, "base body\n");
    }

    #[test]
    fn load_reports_parser_errors() {
        let defaults = tempdir().unwrap();
        fs::write(defaults.path().join("bad.md"), "bad content\n").unwrap();
        assert!(MemoryCatalog::load(defaults.path(), None, &serde_json::json!({})).is_err());
    }

    #[test]
    fn template_memories_render_when_opted_in() {
        let defaults = tempdir().unwrap();
        fs::write(
            defaults.path().join("templated.md"),
            "+++\ntitle = \"Templated\"\ntags = [\"template\"]\ntemplate = true\n+++\nversion={{ version }}\n",
        )
        .unwrap();

        let catalog = MemoryCatalog::load(
            defaults.path(),
            None,
            &serde_json::json!({"version": "1.2.3"}),
        )
        .unwrap();

        assert_eq!(
            catalog.get("sm-templated").unwrap().content,
            "version=1.2.3\n"
        );
    }

    #[test]
    fn template_memories_error_on_invalid_tera() {
        let defaults = tempdir().unwrap();
        fs::write(
            defaults.path().join("bad-template.md"),
            "+++\ntitle = \"Bad\"\ntags = [\"template\"]\ntemplate = true\n+++\nHello {{ version\n",
        )
        .unwrap();

        let err = MemoryCatalog::load(
            defaults.path(),
            None,
            &serde_json::json!({"version": "1.2.3"}),
        )
        .unwrap_err();

        assert!(err.to_string().contains("rendering system memory template"));
    }

    #[test]
    fn literal_tera_braces_are_preserved_without_template_flag() {
        let defaults = tempdir().unwrap();
        fs::write(
            defaults.path().join("literal.md"),
            "+++\ntitle = \"Literal\"\ntags = [\"template\"]\ntemplate = false\n+++\nDo not render {{ version }} here.\n",
        )
        .unwrap();

        let catalog = MemoryCatalog::load(
            defaults.path(),
            None,
            &serde_json::json!({"version": "1.2.3"}),
        )
        .unwrap();

        assert_eq!(
            catalog.get("sm-literal").unwrap().content,
            "Do not render {{ version }} here.\n"
        );
    }

    #[test]
    fn format_catalog_summary_is_metadata_only() {
        let defaults = tempdir().unwrap();
        fs::write(
            defaults.path().join("secret.md"),
            "+++\ntitle = \"Secret\"\ntags = [\"summary\"]\n+++\n\nbody that must stay out of summary\n",
        )
        .unwrap();

        let catalog = MemoryCatalog::load(defaults.path(), None, &serde_json::json!({})).unwrap();
        let summary = catalog.format_catalog_summary(None);

        assert!(summary.contains("sm-secret"));
        assert!(summary.contains("Secret"));
        assert!(summary.contains("tags: summary"));
        assert!(!summary.contains("body that must stay out of summary"));
    }

    #[test]
    fn load_all_shipped_files_parse() {
        let defaults = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("system-defaults")
            .join("memories");
        let dir_iter = fs::read_dir(&defaults).unwrap();
        for path in dir_iter {
            let path = path.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "system-memory-catalog.md")
            {
                continue;
            }
            let body = fs::read_to_string(&path).unwrap();
            let slug = path.file_stem().unwrap().to_str().unwrap();
            crate::system_memory::loader::parse_memory_file(slug, &body).unwrap();
        }
    }

    #[test]
    fn search_finds_by_tag_query() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("packet"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_finds_by_id_query() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("sm-rule-packets"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sm-rule-packets");
    }

    #[test]
    fn search_exact_canonical_id_does_not_expand_prefix_family() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("sm-refactor"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "sm-refactor");

        let quoted = catalog.search(Some("\"sm-refactor\""));
        assert_eq!(quoted.len(), 1);
        assert_eq!(quoted[0].id, "sm-refactor");
    }

    #[test]
    fn search_bare_slug_still_behaves_as_search_term() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("refactor"));
        assert!(hits.iter().any(|m| m.id == "sm-refactor"));
        assert!(hits.iter().any(|m| m.id == "sm-refactor-rust"));
    }

    #[test]
    fn search_finds_by_title_query() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("rule-packets"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_finds_by_body_content() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("generating function"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
        let review_hits = catalog.search(Some("adversarial"));
        assert!(review_hits.iter().any(|m| m.id == "sm-review-packets"));
    }

    #[test]
    fn search_finds_refactor_language_memories() {
        let catalog = fixture_default_catalog();
        let catalog_hits = catalog.search(Some("refactor support matrix"));
        assert!(catalog_hits.iter().any(|m| m.id == "sm-refactor"));

        let rust_hits = catalog.search(Some("rust refactor extract_rust_items"));
        assert!(rust_hits.iter().any(|m| m.id == "sm-refactor-rust"));

        let ts_hits = catalog.search(Some("typescript refactor tsserver"));
        assert!(ts_hits.iter().any(|m| m.id == "sm-refactor-typescript"));

        let csharp_hits = catalog.search(Some("csharp refactor roslyn"));
        assert!(csharp_hits.iter().any(|m| m.id == "sm-refactor-csharp"));

        let python_hits = catalog.search(Some("python refactor pyright"));
        assert!(python_hits.iter().any(|m| m.id == "sm-refactor-python"));

        let java_hits = catalog.search(Some("java refactor jdt"));
        assert!(java_hits.iter().any(|m| m.id == "sm-refactor-java"));

        let go_hits = catalog.search(Some("go refactor gopls"));
        assert!(go_hits.iter().any(|m| m.id == "sm-refactor-go"));

        let cpp_hits = catalog.search(Some("cpp refactor clangd"));
        assert!(cpp_hits.iter().any(|m| m.id == "sm-refactor-c-cpp"));
    }

    #[test]
    fn search_case_insensitive() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("PACKET"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_defaults_adjacent_terms_to_or() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("adversarial rubric"));
        assert!(hits.iter().any(|m| m.id == "sm-review-packets"));
        assert!(hits.iter().any(|m| m.id == "sm-rule-packets"));
    }

    #[test]
    fn search_honors_and_and_exclusion() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("packets AND review -access-table"));
        assert!(hits.iter().any(|m| m.id == "sm-review-packets"));
        assert!(!hits.iter().any(|m| m.id == "sm-auth-packets"));
    }

    #[test]
    fn search_empty_returns_all() {
        let catalog = fixture_default_catalog();
        let all = catalog.search(None);
        assert_eq!(all.len(), catalog.memories.len());
        let empty = catalog.search(Some(""));
        assert_eq!(empty.len(), catalog.memories.len());
    }

    #[test]
    fn get_accepts_canonical_and_bare() {
        let catalog = fixture_default_catalog();
        assert!(catalog.get("sm-rule-packets").is_some());
        assert!(catalog.get("rule-packets").is_some());
        assert!(catalog.get("nonexistent").is_none());
    }

    #[test]
    fn format_for_listing_has_system_prefix() {
        let catalog = fixture_default_catalog();
        let memory = catalog.get("sm-rule-packets").unwrap();
        let out = format_for_listing(memory);
        assert!(out.starts_with("[system] sm-rule-packets"));
        assert!(out.contains("Rule-packets"));
    }

    #[test]
    fn format_for_signpost_is_compact_with_retrieval_breadcrumb() {
        let catalog = fixture_default_catalog();
        let memory = catalog.get("sm-refactor").unwrap();
        let out = format_for_signpost(memory);

        // Header + breadcrumb present.
        assert!(out.starts_with("[system] sm-refactor"));
        assert!(
            out.contains("→ full runbook: bbox_knowledge(query=\"sm-refactor\")"),
            "signpost must point at the exact-id retrieval path: {out}"
        );

        // Compact: a signpost is a handful of lines, never the full body.
        let full = format_for_listing(memory);
        assert!(
            out.len() < full.len(),
            "signpost ({} bytes) should be smaller than full body ({} bytes)",
            out.len(),
            full.len()
        );
        assert!(
            out.lines().count() <= 4,
            "signpost should be at most header+tags+preview+breadcrumb: {out}"
        );
    }

    #[test]
    fn signposts_keep_broad_surfacing_bounded() {
        // Mirrors the broad bbox_knowledge path: a fuzzy term matches many
        // runbooks. Rendering every match as a signpost must stay far smaller
        // than dumping every full body (the overflow the fix targets).
        let catalog = fixture_default_catalog();
        let matches = catalog.search(Some("refactor"));
        assert!(matches.len() > 1, "expected several refactor runbooks");

        let signposts: usize = matches.iter().map(|m| format_for_signpost(m).len()).sum();
        let bodies: usize = matches.iter().map(|m| format_for_listing(m).len()).sum();

        assert!(
            signposts * 4 < bodies,
            "signpost surfacing ({signposts} bytes) should be a small fraction of full bodies ({bodies} bytes)"
        );
        // Even the whole catalog as signposts (the empty-query worst case)
        // stays cheap — vs ~250KB of full bodies and the ~81KB overflow seen
        // on a real broad query.
        let all: usize = catalog
            .memories
            .iter()
            .map(|m| format_for_signpost(m).len())
            .sum();
        assert!(all < 16_000, "all signposts should fit a small budget: {all} bytes");
    }

    #[test]
    fn signpost_preview_strips_markers_and_truncates() {
        // A heading line's text makes a fine preview once its `#` markers are
        // stripped.
        let preview = signpost_preview("## Bro orchestration hygiene\n\nbody").expect("preview");
        assert_eq!(preview, "Bro orchestration hygiene");

        // A long first line is truncated at a byte budget with an ellipsis.
        let long = "x".repeat(SIGNPOST_PREVIEW_BYTES + 50);
        let truncated = signpost_preview(&format!("- {long}")).expect("preview");
        assert!(truncated.ends_with('…'), "long preview should be truncated");
        assert!(truncated.len() <= SIGNPOST_PREVIEW_BYTES + 4);
    }

    #[test]
    fn no_duplicate_ids() {
        let catalog = fixture_default_catalog();
        let mut ids: Vec<_> = catalog.memories.iter().map(|m| &m.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate ids in catalog");
    }

    #[test]
    fn all_ids_canonical_prefix() {
        let catalog = fixture_default_catalog();
        for memory in &catalog.memories {
            assert!(memory.id.starts_with("sm-"));
        }
    }

    #[test]
    fn rule_packets_memory_loaded_and_nonempty() {
        let catalog = fixture_default_catalog();
        let memory = catalog
            .get("sm-rule-packets")
            .expect("sm-rule-packets must exist");
        assert!(memory.content.len() > 500);
        assert!(memory.content.contains("bbox_compile"));
        assert!(memory.content.contains("bbox_apply"));
        assert!(memory.content.contains("bbox_audit"));
    }

    #[test]
    fn gap_notes_memory_loaded_and_teaches_bbox_gap() {
        let catalog = fixture_default_catalog();
        let memory = catalog
            .get("sm-gap-notes")
            .expect("sm-gap-notes must exist");
        assert!(memory.content.len() > 500);
        // First-class surface: filing is via `bbox_gap`, dedupe via `bbox_gaps`.
        assert!(memory.content.contains("bbox_gap("));
        assert!(memory.content.contains("bbox_gaps"));
        // The `blackbox.gap_note.v1` type tag survives for the file-drop spool path.
        assert!(memory.content.contains("blackbox.gap_note.v1"));
    }

    #[test]
    fn search_finds_gap_notes_by_subject_vocabulary() {
        let catalog = fixture_default_catalog();
        let hits = catalog.search(Some("substrate gap"));
        assert!(hits.iter().any(|m| m.id == "sm-gap-notes"));

        let envelope_hits = catalog.search(Some("blackbox.gap_note.v1"));
        assert!(envelope_hits.iter().any(|m| m.id == "sm-gap-notes"));

        let id_hits = catalog.search(Some("sm-gap-notes"));
        assert_eq!(id_hits.len(), 1);
        assert_eq!(id_hits[0].id, "sm-gap-notes");
    }

    #[test]
    fn gap_notes_memory_is_distinct_from_side_channel_notes() {
        let catalog = fixture_default_catalog();
        let gap = catalog
            .get("sm-gap-notes")
            .expect("sm-gap-notes must exist");
        let side = catalog
            .get("sm-side-channel-notes")
            .expect("sm-side-channel-notes must exist");
        assert_ne!(gap.id, side.id);
        assert_ne!(gap.content, side.content);
    }
}
