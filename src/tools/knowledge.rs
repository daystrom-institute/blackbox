use std::collections::BTreeMap;

use crate::knowledge::{
    DecideParams, ForgetParams, KnowledgeLinkParams, KnowledgeListParams, LearnParams,
    RememberParams, ResponseFormat,
};
use crate::packets::packet_matches_query;
use crate::server::BlackboxServer;
use crate::system_memory;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::json;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::knowledge_tools()
}

fn has_runtime_knowledge_filter(p: &KnowledgeListParams) -> bool {
    p.scope.is_some()
        || p.project.is_some()
        || p.provider.is_some()
        || p.status.is_some()
        || p.approval.is_some()
}

/// Extract the top knowledge entry id from a `kb.list` entries block for the
/// response breadcrumb. The block opens with `N entries:\n\n[<id>] …`, so the
/// first bracketed token is the highest-ranked entry. Returns None for the
/// "No entries found." sentinel (no `[`), so a packet-only or memory-only
/// response does not emit a spurious entry pointer.
fn first_entry_id(entries_block: &str) -> Option<String> {
    let start = entries_block.find('[')? + 1;
    let end = entries_block[start..].find(']')? + start;
    let id = entries_block[start..end].trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn entry_ids(entries_block: &str) -> Vec<String> {
    entries_block
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('[')?;
            let end = rest.find(']')?;
            let id = rest[..end].trim();
            (!id.is_empty()).then(|| id.to_string())
        })
        .collect()
}

fn log_tool_ok(tool: &'static str, start: std::time::Instant, bytes: usize) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes, "ok");
}

fn log_tool_err(tool: &'static str, start: std::time::Instant, err: &anyhow::Error) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %err, "err");
}

/// Rescope an absolute-path `project` filter through worktree→base project
/// resolution. A managed/linked worktree (or a subdirectory) of a registered
/// project hashes/keys differently from the base, so the literal path would
/// silently match nothing; entries live under the registered base path.
/// Rewrites `p.project` to the base canonical path and records the worktree
/// checkout root in `p.project_alias` so entries written from inside the
/// worktree (scoped to the worktree path) stay visible too. Non-path filters
/// (substring matches like "transcript-search") and unregistered paths are
/// left untouched.
fn rescope_project_filter(
    p: &mut KnowledgeListParams,
    projects: &[crate::projects::ProjectRecord],
) {
    let Some(raw) = p.project.as_deref().filter(|raw| raw.starts_with('/')) else {
        return;
    };
    let Some((scope, checkout)) = crate::projects::resolve_scope_and_checkout_dir(raw, projects)
    else {
        return;
    };
    if checkout != scope {
        p.project_alias = Some(checkout);
    }
    p.project = Some(scope);
}

impl BlackboxServer {
    /// Rescope a knowledge WRITE's `project` param through worktree→base
    /// resolution: the entry's durable scope becomes the registered base
    /// (so render/list/inject filters keyed by the base path match it), and
    /// the returned write-dir — `Some` only for a recognized worktree —
    /// redirects the repo-owned `.bbox/knowledge/` file into the caller's
    /// checkout so it travels with the branch (gap-de82a74d). Absence of
    /// `project` means GLOBAL write scope (tool-arg-defaulting §3.1) and is
    /// never touched; empty/whitespace values are left for store validation.
    fn rescope_knowledge_write(&self, project: &mut Option<String>) -> Option<String> {
        let raw = project.clone().filter(|s| !s.trim().is_empty())?;
        let (scope, write_dir) = self.resolve_project_write_scope(&raw);
        *project = Some(scope);
        write_dir
    }
}

fn matches_system_memory_catalog(category: Option<&str>) -> bool {
    matches!(
        category,
        Some("system_memory") | Some("system-memory") | Some("system_memories")
    )
}

fn format_system_memory_catalog(query: Option<&str>) -> String {
    system_memory::format_catalog_summary(query)
}

fn exact_system_memory_response(p: &KnowledgeListParams) -> Option<String> {
    if has_runtime_knowledge_filter(p) {
        return None;
    }
    if matches_system_memory_catalog(p.category.as_deref()) {
        return Some(format_system_memory_catalog(p.query.as_deref()));
    }
    if p.category.is_some() {
        return None;
    }
    let memory = system_memory::exact_query(p.query.as_deref())?;
    let mut out = String::new();
    out.push_str("── System memories ──────────────────────────\n");
    out.push_str(&system_memory::format_for_listing(memory));
    Some(out)
}

#[tool_router(router = knowledge_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_learn",
        description = "Persist a user-stated rule or convention that should bind future sessions; rendered into provider markdown files. Use for narrative rules (\"we always X\", \"never Y\"). If the rule you're storing is actually a priority-ordered decision function, classification rubric, or structured mechanism — use `bbox_compile` instead; that produces a shareable packet any agent can apply deterministically."
    )]
    pub(crate) async fn bbox_learn(
        &self,
        Parameters(p): Parameters<LearnParams>,
    ) -> CallToolResult {
        let format = match ResponseFormat::parse_optional(p.format.as_deref()) {
            Ok(format) => format,
            Err(e) => return Self::err_text(&format!("Error: {e:#}")),
        };
        let warning = self.arc_bound_warning(p.id.as_deref(), &p.content);
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            let write_dir = server.rescope_knowledge_write(&mut p.project);
            let mut kb = server.state.kb.write();
            let result = kb.learn_result_with_write_dir(&p, false, write_dir.as_deref())?;
            let rider = kb.repo_record_rider(&result.id);
            Ok::<_, anyhow::Error>((result, rider))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_learn", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                match format {
                    ResponseFormat::Text => {
                        let mut text = match warning {
                            Some(w) => format!("{}{}", result.message, w),
                            None => result.message,
                        };
                        if let Some(rider) = &rider {
                            text.push_str(rider);
                        }
                        log_tool_ok("bbox_learn", start, text.len());
                        Self::ok_text(&text)
                    }
                    ResponseFormat::Json => {
                        let message = match &rider {
                            Some(rider) => format!("{}{}", result.message, rider),
                            None => result.message,
                        };
                        let mut payload = serde_json::json!({
                            "id": result.id,
                            "action": result.action,
                            "rendered": result.rendered,
                            "render_pending": result.render_pending,
                            "message": message,
                        });
                        if let Some(summary) = result.summary {
                            payload["summary"] = serde_json::json!(summary);
                        }
                        if let Some(w) = warning {
                            payload["warnings"] = serde_json::json!([w.trim().to_string()]);
                        }
                        let bytes = serde_json::to_string(&payload)
                            .map(|s| s.len())
                            .unwrap_or_default();
                        log_tool_ok("bbox_learn", start, bytes);
                        Self::ok_json(&payload)
                    }
                }
            }
            Err(e) => {
                log_tool_err("bbox_learn", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_remember",
        description = "Persist a fact for later recall; indexed but NOT rendered."
    )]
    pub(crate) async fn bbox_remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            let write_dir = server.rescope_knowledge_write(&mut p.project);
            let mut kb = server.state.kb.write();
            let result = kb.remember_result_with_write_dir(&p, false, write_dir.as_deref())?;
            let rider = kb.repo_record_rider(&result.id);
            Ok::<_, anyhow::Error>((result, rider))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_remember", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                log_tool_ok("bbox_remember", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_remember", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_decide",
        description = "Record a durable commitment with required rationale; supports supersession."
    )]
    pub(crate) async fn bbox_decide(
        &self,
        Parameters(p): Parameters<DecideParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            let write_dir = server.rescope_knowledge_write(&mut p.project);
            let mut kb = server.state.kb.write();
            let result = kb.decide_result_with_write_dir(&p, false, write_dir.as_deref())?;
            let rider = kb.repo_record_rider(&result.id);
            Ok::<_, anyhow::Error>((result, rider))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_decide", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                if let Some(old_id) = result.superseded.as_deref() {
                    if let Err(err) = self.tombstone_knowledge_entry_in_index(old_id) {
                        tracing::warn!(error = %err, entry = %old_id, "knowledge index tombstone failed; will reconstruct on next reindex cycle");
                    }
                }
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                log_tool_ok("bbox_decide", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_decide", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_knowledge",
        description = "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces matching rule-packets and system memories; system memories include system_memory:<id> refs usable with bbox_inspect_entity or bbox_bundle_evidence. Pass category=\"packet\" to list compiled packets, category=\"system_memory\" to list memory metadata, or bbox_packet_list for structured packet filters."
    )]
    pub(crate) async fn bbox_knowledge(
        &self,
        Parameters(p): Parameters<KnowledgeListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_knowledge", move || {
            if let Some(out) = exact_system_memory_response(&p) {
                return Ok(out);
            }

            let mut p = p;
            if p.project.is_some() {
                let projects = server.state.projects.read().list();
                rescope_project_filter(&mut p, &projects);
            }

            let mut combined = server.state.kb.write().list(&p)?;
            // Captured before packets/memories are appended, so it reflects the
            // top knowledge entry (not a packet/memory line).
            let top_entry_id = first_entry_id(&combined);
            let recall_ids = entry_ids(&combined);
            if !recall_ids.is_empty() {
                let recall_result = {
                    let mut kb = server.state.kb.write();
                    kb.record_recall(&recall_ids)
                };
                match recall_result {
                    Ok(()) => {
                        // Recall telemetry was always best-effort on this read path;
                        // keep it write-behind rather than making queries wait for fsync.
                        server.state.kb_persister.request();
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "knowledge recall telemetry update failed");
                    }
                }
            }

            // Surface matching packets. Uses the same match semantics as
            // bbox_packet_list so the two tools agree on what "matches" means.
            let all_packets = server.state.packets.read().list_all()?;
            let matching_packets: Vec<_> =
                if let Some(q) = p.query.as_deref().filter(|q| !q.is_empty()) {
                    all_packets
                        .into_iter()
                        .filter(|pkt| packet_matches_query(pkt, q))
                        .collect()
                } else if p.category.as_deref() == Some("packet") {
                    all_packets
                } else {
                    Vec::new()
                };

            if !matching_packets.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Rule-packets ───────────────────────────────\n");
                let limit = p.limit.unwrap_or(50).min(500) as usize;
                for pkt in matching_packets.iter().take(limit) {
                    let histogram: Vec<String> = pkt
                        .rules
                        .iter()
                        .fold(BTreeMap::<String, usize>::new(), |mut acc, r| {
                            *acc.entry(r.classification.clone()).or_insert(0) += 1;
                            acc
                        })
                        .into_iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect();
                    combined.push_str(&format!(
                        "[{}] Packet | domain: {} | scope: {} | {} rules [{}] | created {}\n",
                        pkt.id,
                        pkt.domain,
                        pkt.scope,
                        pkt.rules.len(),
                        histogram.join(", "),
                        pkt.created_at,
                    ));
                }
                combined.push_str(
                    "  (use bbox_packet_list for filter/query/preview; bbox_apply to evaluate)\n",
                );
            }

            // Also surface matching system memories. See
            // system-defaults/memories/ — these are file-loaded runbooks
            // read at startup, queryable but never rendered.
            //
            // The broad query path renders compact signposts, not full bodies:
            // a fuzzy multi-term query matches many runbooks, and full bodies
            // (~40KB each) overflow the token budget. The agent pulls a full
            // body via the exact-id short-circuit (`bbox_knowledge(query="sm-…")`,
            // handled above by exact_system_memory_response).
            let memories = system_memory::search(p.query.as_deref());
            if !memories.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── System memories ──────────────────────────\n");
                for m in memories {
                    combined.push_str(&system_memory::format_for_signpost(m));
                    combined.push('\n');
                }
                combined.push_str(
                    "  (signposts only — query an exact sm-* id for the full runbook body)\n",
                );
            }

            // Top-level breadcrumb: pull the highest-ranked knowledge entry into
            // the graph funnel. Packets and memories carry their own pointers
            // above; this completes the response-breadcrumb plane for entries.
            if let Some(id) = &top_entry_id {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Next steps ───────────────────────────────\n");
                combined.push_str(&format!(
                    "  → Inspect the top entry's edges + provenance: bbox_inspect_entity(entity_ref=\"knowledge:{id}\")\n"
                ));
                combined.push_str(&format!(
                    "  → Package an answer: bbox_bundle_evidence(question=<q>, entity_refs=[\"knowledge:{id}\"])\n"
                ));
            }
            Ok(combined)
        })
        .await
    }

    #[tool(name = "bbox_knowledge_link", description = "Append a knowledge edge.")]
    pub(crate) async fn bbox_knowledge_link(
        &self,
        Parameters(p): Parameters<KnowledgeLinkParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let edge = server.state.kb.write().append_link(&p)?;
            Ok::<_, anyhow::Error>(serde_json::to_string_pretty(&json!({
                "status": "linked",
                "source": p.source,
                "target": p.target,
                "kind": edge.kind.edge_kind(),
                "confidence": edge.confidence,
            }))?)
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge link task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok(text) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_knowledge_link", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                log_tool_ok("bbox_knowledge_link", start, text.len());
                Self::ok_text(&text)
            }
            Err(e) => {
                log_tool_err("bbox_knowledge_link", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(name = "bbox_forget", description = "Retire or supersede an entry.")]
    pub(crate) async fn bbox_forget(
        &self,
        Parameters(p): Parameters<ForgetParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let id = p.id.clone();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || server.state.kb.write().forget(&p))
            .await
            .map_err(|e| anyhow::anyhow!("knowledge forget task failed: {e}"))
            .and_then(std::convert::identity);

        match write_result {
            Ok(message) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_forget", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.tombstone_knowledge_entry_in_index(&id) {
                    tracing::warn!(error = %err, entry = %id, "knowledge index tombstone failed; will reconstruct on next reindex cycle");
                }
                log_tool_ok("bbox_forget", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_forget", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_system_memory() {
        crate::init_system_memory_for_tests();
    }

    #[test]
    fn first_entry_id_extracts_top_and_handles_sentinel() {
        let block = "2 entries:\n\n[abc123] Convention/project | all | title\n  \
                     content_bytes=10\n  body [with brackets]\n\n[def456] ...";
        assert_eq!(first_entry_id(block).as_deref(), Some("abc123"));
        assert_eq!(first_entry_id("No entries found."), None);
        assert_eq!(first_entry_id(""), None);
    }

    #[test]
    fn exact_system_memory_response_returns_only_exact_memory() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            query: Some("sm-refactor".into()),
            ..Default::default()
        })
        .expect("exact canonical system memory query should short-circuit");

        assert!(out.contains("[system] sm-refactor"));
        assert!(!out.contains("[system] sm-refactor-rust"));
        assert!(!out.contains("[bb-tool-reference]"));
        assert!(!out.contains("No entries found."));
    }

    #[test]
    fn exact_system_memory_response_respects_runtime_filters() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            scope: Some("project".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });

        assert!(out.is_none());
    }

    #[test]
    fn system_memory_catalog_returns_all_memories() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("system_memory".into()),
            ..Default::default()
        })
        .expect("system_memory category should return catalog");

        assert!(out.contains("── System memories"));
        assert!(out.contains("[system] sm-rule-packets"));
        assert!(out.contains("[system] sm-refactor"));
        assert!(out.contains("[system] sm-agentic-opening-sequence"));
        assert!(
            !out.contains("bbox_compile"),
            "catalog listing should not include full body"
        );
    }

    #[test]
    fn system_memory_catalog_accepts_hyphenated_and_plural_forms() {
        init_system_memory();
        for form in &["system_memory", "system-memory", "system_memories"] {
            let out = exact_system_memory_response(&KnowledgeListParams {
                category: Some(form.to_string()),
                ..Default::default()
            });
            assert!(out.is_some(), "category={} should match", form);
        }
    }

    #[test]
    fn system_memory_catalog_supports_query_filter() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("system_memory".into()),
            query: Some("refactor".into()),
            ..Default::default()
        })
        .expect("system_memory + query should return filtered catalog");

        assert!(out.contains("[system] sm-refactor"));
        assert!(out.contains("[system] sm-refactor-rust"));
        assert!(!out.contains("[system] sm-rule-packets"));
    }

    #[test]
    fn system_memory_category_does_not_match_memory() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("memory".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });
        assert!(out.is_none());
    }

    fn init_repo_with_worktree(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::process::Command;
        let base = tmp.join("repo");
        std::fs::create_dir_all(&base).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Blackbox Test",
                "-c",
                "user.email=blackbox@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(&base)
                .args(&args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let worktree = tmp.join("wt");
        let out = Command::new("git")
            .arg("-C")
            .arg(&base)
            .args([
                "worktree",
                "add",
                "-b",
                "arc/scoped",
                worktree.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            base.canonicalize().unwrap(),
            worktree.canonicalize().unwrap(),
        )
    }

    fn record_for(path: &std::path::Path) -> crate::projects::ProjectRecord {
        crate::projects::ProjectRecord {
            project_id: "feedbeef".into(),
            repo_id: None,
            canonical_path: path.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
        }
    }

    #[test]
    fn rescope_project_filter_resolves_worktree_to_base_with_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, worktree) = init_repo_with_worktree(&tmp_root);
        let projects = vec![record_for(&base)];

        let mut p = KnowledgeListParams {
            project: Some(worktree.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_project_filter(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_alias.as_deref(), Some(worktree.to_str().unwrap()));

        // A plain descendant collapses to the base with no alias needed
        // (descendant entry paths contain the base path already).
        let subdir = base.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let mut p = KnowledgeListParams {
            project: Some(subdir.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_project_filter(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_alias, None);
    }

    #[test]
    fn rescope_project_filter_leaves_non_path_and_unregistered_filters_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let projects = vec![record_for(&base)];

        // Substring filter (not an absolute path) is untouched.
        let mut p = KnowledgeListParams {
            project: Some("transcript-search".into()),
            ..Default::default()
        };
        rescope_project_filter(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some("transcript-search"));
        assert_eq!(p.project_alias, None);

        // An absolute path no registered project owns is untouched.
        let stranger = tmp_root.join("stranger");
        std::fs::create_dir_all(&stranger).unwrap();
        let mut p = KnowledgeListParams {
            project: Some(stranger.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_project_filter(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some(stranger.to_str().unwrap()));
        assert_eq!(p.project_alias, None);
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// End-to-end repro of gap-de82a74d: an agent inside an in-tree linked
    /// worktree (`<root>/.claude/worktrees/<name>`) learns a project-scoped
    /// entry. The entry must key to the registered BASE (durable scope), the
    /// committed `.bbox/knowledge/` file must land in the WORKTREE (travels
    /// with the branch, never mutates the base checkout), and an immediate
    /// `bbox_render` from the same worktree must include the entry — the
    /// asymmetry that motivated the gap.
    #[tokio::test]
    async fn bbox_learn_from_worktree_keys_base_writes_worktree_and_renders() {
        use crate::knowledge::RenderParams;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        // Repo-owned: the checkout (and thus the worktree) carries .bbox/knowledge/.
        std::fs::create_dir_all(base.join(".bbox").join("knowledge")).unwrap();
        std::fs::write(base.join(".bbox").join("knowledge").join(".gitkeep"), "").unwrap();
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &["worktree", "add", "-b", "arc/kb", worktree.to_str().unwrap(), "HEAD"],
        );
        let wt_canon = worktree.canonicalize().unwrap();
        let wt = wt_canon.to_string_lossy().into_owned();

        let server = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(tmp.path()),
        ));
        server
            .state
            .projects
            .write()
            .register_path(&base_canon)
            .unwrap();

        let learn = server
            .bbox_learn(Parameters(LearnParams {
                content: "WORKTREE_KB_MARKER: prefer rustls".into(),
                category: "convention".into(),
                scope: Some("project".into()),
                project: Some(wt.clone()),
                ..Default::default()
            }))
            .await;
        assert_ne!(learn.is_error, Some(true), "learn failed: {learn:?}");

        // Durable scope = registered base; committed file = worktree checkout.
        let (id, project) = {
            let kb = server.state.kb.read();
            let entry = kb
                .all_entries()
                .iter()
                .find(|e| e.content.contains("WORKTREE_KB_MARKER"))
                .expect("entry stored");
            (entry.id.clone(), entry.project.clone())
        };
        assert_eq!(
            project.as_deref(),
            Some(base_canon.to_string_lossy().as_ref()),
            "entry must key to the registered base, not the worktree"
        );
        let rel = std::path::Path::new(".bbox")
            .join("knowledge")
            .join(format!("{id}.json"));
        assert!(
            wt_canon.join(&rel).exists(),
            "committed entry file must land in the worktree"
        );
        assert!(
            !base_canon.join(&rel).exists(),
            "the daemon must not mutate the base checkout"
        );

        // The other half of the gap: render from the worktree sees the entry.
        let render = server
            .bbox_render(Parameters(RenderParams {
                provider: Some("claude".into()),
                project: Some(wt.clone()),
                scope: Some("project".into()),
                dry_run: Some(false),
                ..Default::default()
            }))
            .await;
        assert_ne!(render.is_error, Some(true), "render failed: {render:?}");
        let rendered = std::fs::read_to_string(wt_canon.join("CLAUDE.md")).unwrap();
        assert!(
            rendered.contains("WORKTREE_KB_MARKER"),
            "worktree render must include the just-learned entry: {rendered}"
        );
    }
}
