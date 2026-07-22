//! `bbox_doctor` v0: one read-only "what do I need to know right now?"
//! surface (design/operations/config-artifacts/ops-artifact-bundles-and-doctor.md,
//! Phase 5 pulled forward). Aggregates existing health signals in-process
//! and classifies findings; it never mutates stores, enqueues notes, or
//! emits inbox items.
//!
//! v0 ships the substrate-independent sections only: daemon, index,
//! vectors, graph, projects, memories, knowledge, attention. The
//! artifact/inlet/workflow drift sections stay deferred until the bundle
//! and activator phases land (they need catalog machinery that does not
//! exist yet).

use serde::Serialize;

/// Finding severity, ordered so `max()` yields the worst level for the
/// report status line. `Ok < Info < Warn < Action < Blocked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FindingLevel {
    Ok,
    Info,
    Warn,
    Action,
    Blocked,
}

impl FindingLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Action => "action",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Finding {
    pub(crate) level: FindingLevel,
    pub(crate) message: String,
    /// Suggested next command when one exists (`action` findings should
    /// almost always carry one; `ok` findings never do).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
}

impl Finding {
    pub(crate) fn ok(message: impl Into<String>) -> Self {
        Self {
            level: FindingLevel::Ok,
            message: message.into(),
            next: None,
        }
    }

    pub(crate) fn info(message: impl Into<String>) -> Self {
        Self {
            level: FindingLevel::Info,
            message: message.into(),
            next: None,
        }
    }

    pub(crate) fn warn(message: impl Into<String>) -> Self {
        Self {
            level: FindingLevel::Warn,
            message: message.into(),
            next: None,
        }
    }

    pub(crate) fn action(message: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            level: FindingLevel::Action,
            message: message.into(),
            next: Some(next.into()),
        }
    }

    pub(crate) fn blocked(message: impl Into<String>) -> Self {
        Self {
            level: FindingLevel::Blocked,
            message: message.into(),
            next: None,
        }
    }

    pub(crate) fn with_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SectionReport {
    pub(crate) section: &'static str,
    pub(crate) findings: Vec<Finding>,
}

impl SectionReport {
    pub(crate) fn worst(&self) -> FindingLevel {
        self.findings
            .iter()
            .map(|f| f.level)
            .max()
            .unwrap_or(FindingLevel::Ok)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) status: FindingLevel,
    pub(crate) sections: Vec<SectionReport>,
}

impl DoctorReport {
    pub(crate) fn from_sections(sections: Vec<SectionReport>) -> Self {
        let status = sections
            .iter()
            .map(SectionReport::worst)
            .max()
            .unwrap_or(FindingLevel::Ok);
        Self { status, sections }
    }

    /// Compact operator-facing text: status line, then findings grouped
    /// worst-first with their suggested next commands, then a one-line
    /// per-section ok roll-up. Mirrors the design doc's example summary.
    pub(crate) fn render_summary(&self) -> String {
        let mut out = format!("status: {}\n", self.status.as_str());
        for level in [
            FindingLevel::Blocked,
            FindingLevel::Action,
            FindingLevel::Warn,
            FindingLevel::Info,
        ] {
            let group: Vec<(&str, &Finding)> = self
                .sections
                .iter()
                .flat_map(|s| s.findings.iter().map(move |f| (s.section, f)))
                .filter(|(_, f)| f.level == level)
                .collect();
            if group.is_empty() {
                continue;
            }
            out.push_str(&format!("\n{}:\n", level.as_str()));
            for (section, finding) in group {
                out.push_str(&format!("- [{section}] {}\n", finding.message));
                if let Some(next) = &finding.next {
                    out.push_str(&format!("  next: {next}\n"));
                }
            }
        }
        let ok_sections: Vec<&str> = self
            .sections
            .iter()
            .filter(|s| s.worst() == FindingLevel::Ok)
            .map(|s| s.section)
            .collect();
        if !ok_sections.is_empty() {
            out.push_str(&format!("\nok: {}\n", ok_sections.join(", ")));
        }
        out
    }
}

/// Collect the full v0 report. Read-only: every section takes short read
/// guards on existing stores; nothing here mutates state, enqueues work,
/// or writes attention items. Runs on the blocking pool (store reads and
/// path probes are blocking I/O).
pub(crate) fn run(server: &crate::server::BlackboxServer) -> anyhow::Result<DoctorReport> {
    let state = &server.state;
    let sections = vec![
        daemon_section(state),
        index_section(state),
        code_sources_section(state),
        vectors_section(state),
        graph_section(server),
        projects_section(state),
        memories_section(state),
        knowledge_section(state),
        attention_section(state),
    ];
    Ok(DoctorReport::from_sections(sections))
}

fn code_sources_section(state: &crate::server::state::SharedState) -> SectionReport {
    let store = state.code_sources.store();
    let mut findings = Vec::new();
    match store.health_records() {
        Ok(records) => {
            for record in records {
                let finding = match record.code.as_str() {
                    "preservation_failed" => Finding::blocked(format!(
                        "project `{}` full-rebuild preservation failed: {}",
                        record.project_id, record.diagnostic
                    ))
                    .with_next(
                        "re-ship the active generation or remove its collector assignment to complete an explicit local cutback",
                    ),
                    "missing_blob_data" => Finding::blocked(format!(
                        "project `{}` has missing or corrupt collected blobs: {}",
                        record.project_id, record.diagnostic
                    ))
                    .with_next(
                        "re-run the collector to repair the generation, or remove its assignment for an explicit local cutback",
                    ),
                    "cutback_pending" => Finding::action(
                        format!(
                            "project `{}` local cutback is pending: {}",
                            record.project_id, record.diagnostic
                        ),
                        "restore the registered checkout and reload configuration to retry cutback",
                    ),
                    "activation_failed" => Finding::action(
                        format!(
                            "project `{}` collected activation failed: {}",
                            record.project_id, record.diagnostic
                        ),
                        "repair the reported source/store condition and publish the checkout again",
                    ),
                    _ => Finding::warn(format!(
                        "project `{}` code-source health issue `{}`: {}",
                        record.project_id, record.code, record.diagnostic
                    )),
                };
                findings.push(finding);
            }
        }
        Err(error) => findings.push(Finding::blocked(format!(
            "code-source health records are unreadable: {error:#}"
        ))),
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let stale_hours = state.config.read().code_collection.stale_warning_hours;
    match store.activation_records() {
        Ok(activations) => {
            for activation in activations {
                let generation = match store.find_generation(&activation.generation_id) {
                    Ok(generation) => generation,
                    Err(error) => {
                        findings.push(Finding::blocked(format!(
                            "project `{}` active collected generation is unreadable: {error:#}",
                            activation.project_id
                        )));
                        continue;
                    }
                };
                let age_hours = now
                    .saturating_sub(activation.activated_unix_secs)
                    .checked_div(3_600)
                    .unwrap_or_default();
                if age_hours >= stale_hours {
                    findings.push(Finding::warn(format!(
                        "project `{}` collected generation is {} hours old",
                        activation.project_id, age_hours
                    )));
                } else if generation.state == bbox_code_source::GenerationState::Active {
                    findings.push(Finding::ok(format!(
                        "project `{}` collected generation active ({} files, {} bytes, age {}h)",
                        activation.project_id,
                        generation.descriptor.file_count,
                        generation.descriptor.logical_bytes,
                        age_hours
                    )));
                }
                if let Some(project) = state
                    .projects
                    .read()
                    .list()
                    .into_iter()
                    .find(|project| project.project_id == activation.project_id)
                {
                    let local_head = bbox_corpus_core::git::current_head(std::path::Path::new(
                        &project.canonical_path,
                    ));
                    if local_head.as_deref() != Some(generation.descriptor.head_commit.as_str()) {
                        findings.push(Finding::warn(format!(
                            "project `{}` local Git-history HEAD differs from collected current files",
                            activation.project_id
                        )));
                    }
                }
            }
        }
        Err(error) => findings.push(Finding::blocked(format!(
            "code-source activation records are unreadable: {error:#}"
        ))),
    }
    if findings.is_empty() {
        findings.push(if state.config.read().code_collection.enabled {
            Finding::info("code collection enabled with no active collected generations")
        } else {
            Finding::ok("code collection disabled; project sources are local")
        });
    }
    SectionReport {
        section: "code_sources",
        findings,
    }
}

fn daemon_section(state: &crate::server::state::SharedState) -> SectionReport {
    let cfg = state.config.read();
    let findings = vec![Finding::ok(format!(
        "blackboxd {} on {}:{} (mcp `{}`); state {}, runtime store {}",
        env!("CARGO_PKG_VERSION"),
        cfg.daemon.bind,
        cfg.daemon.port,
        cfg.daemon.mcp_name,
        cfg.paths.state_dir.display(),
        state.store_dir.display(),
    ))];
    SectionReport {
        section: "daemon",
        findings,
    }
}

fn index_section(state: &crate::server::state::SharedState) -> SectionReport {
    let idx = state.idx.read();
    let num_docs = idx.num_docs();
    let finding = if num_docs == 0 {
        Finding::action(
            "search index is empty",
            "bbox_reindex() to build it (first build can take a while)",
        )
    } else {
        Finding::ok(format!("{num_docs} indexed documents"))
    };
    SectionReport {
        section: "index",
        findings: vec![finding],
    }
}

fn vectors_section(state: &crate::server::state::SharedState) -> SectionReport {
    let findings = match crate::embed_runtime::status_response_for_state(state) {
        Ok(response) => {
            if response.routes.is_empty() {
                vec![Finding::info("no embedding routes active yet")]
            } else {
                response
                    .routes
                    .iter()
                    .map(|(route, status)| classify_embed_route(route, status))
                    .collect()
            }
        }
        Err(err) => vec![Finding::warn(format!(
            "embedding status unavailable: {err:#}"
        ))],
    };
    SectionReport {
        section: "vectors",
        findings,
    }
}

fn graph_section(server: &crate::server::BlackboxServer) -> SectionReport {
    let counts = server.describe_schema_counts();
    let total: usize = counts.values().sum();
    let finding = if counts.is_empty() || total == 0 {
        Finding::info("agentic graph has no entities yet (populated by indexing)")
    } else {
        Finding::ok(format!("{total} entities across {} types", counts.len()))
    };
    SectionReport {
        section: "graph",
        findings: vec![finding],
    }
}

fn projects_section(state: &crate::server::state::SharedState) -> SectionReport {
    let records = state.projects.read().list();
    let mut findings = Vec::new();
    let mut present = 0usize;
    for record in &records {
        if std::path::Path::new(&record.canonical_path).exists() {
            present += 1;
        } else {
            findings.push(
                Finding::warn(format!(
                    "project `{}` path missing on disk: {}",
                    record.project_id, record.canonical_path
                ))
                .with_next(format!(
                    "bbox_project_rename(project=\"{}\", new_path=...) if it moved, or \
                     bbox_project_unregister(project=\"{}\")",
                    record.project_id, record.project_id
                )),
            );
        }
    }
    if findings.is_empty() {
        findings.push(if records.is_empty() {
            Finding::info("no projects registered")
        } else {
            Finding::ok(format!("{present} registered project(s), all present"))
        });
    }
    SectionReport {
        section: "projects",
        findings,
    }
}

fn memories_section(state: &crate::server::state::SharedState) -> SectionReport {
    let cfg = state.config.read();
    let dir = &cfg.paths.defaults_memories_dir;
    let finding = match crate::system_memory::catalog_if_loaded() {
        Some(catalog) => {
            let count = catalog.search(None).len();
            if count == 0 {
                Finding::warn(format!(
                    "system memory catalog loaded but empty (defaults dir: {})",
                    dir.display()
                ))
            } else {
                Finding::ok(format!("{count} system memories loaded"))
            }
        }
        None => Finding::blocked(format!(
            "system memory catalog never initialized (defaults dir: {}); \
             daemon startup likely failed partway",
            dir.display()
        )),
    };
    SectionReport {
        section: "memories",
        findings: vec![finding],
    }
}

fn knowledge_section(state: &crate::server::state::SharedState) -> SectionReport {
    let finding = match state.kb.read().lint() {
        Ok(report) => {
            if report.starts_with("No issues") {
                Finding::ok("knowledge lint clean")
            } else {
                let headline = report.lines().next().unwrap_or("issues found").to_string();
                Finding::warn(format!("knowledge lint: {headline}"))
                    .with_next("bbox_lint() for the full report".to_string())
            }
        }
        Err(err) => Finding::warn(format!("knowledge lint failed: {err:#}")),
    };
    SectionReport {
        section: "knowledge",
        findings: vec![finding],
    }
}

fn attention_section(state: &crate::server::state::SharedState) -> SectionReport {
    use bbox_threads::notes::{NoteKind, NoteResolution};
    let mut findings = Vec::new();

    let notes = state.notes.read();
    let mut by_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for note in notes.all() {
        if note.resolution == NoteResolution::Unresolved {
            let kind: &str = note.kind.as_ref();
            *by_kind.entry(kind.to_string()).or_default() += 1;
        }
    }
    drop(notes);
    if !by_kind.is_empty() {
        let summary = by_kind
            .iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let urgent: usize = [NoteKind::Blocked, NoteKind::Dispute]
            .iter()
            .filter_map(|k| by_kind.get(k.as_ref() as &str))
            .sum();
        let finding = if urgent > 0 {
            Finding::warn(format!(
                "{urgent} unresolved blocked/dispute note(s) (all unresolved: {summary})"
            ))
        } else {
            Finding::info(format!("unresolved notes: {summary}"))
        };
        findings.push(finding.with_next("bbox_inbox() to triage".to_string()));
    }

    let failed_tasks = {
        let task_store = state.task_store.read();
        task_store
            .all_tasks()
            .iter()
            .filter(|t| t.inner.lock().status == crate::orchestration::TaskStatus::Failed)
            .count()
    };
    if failed_tasks > 0 {
        findings.push(
            Finding::warn(format!("{failed_tasks} failed dispatch task(s)"))
                .with_next("bro_dashboard() / bro_status(task_id=..., tail=20)".to_string()),
        );
    }

    if findings.is_empty() {
        findings.push(Finding::ok("no unresolved attention items"));
    }
    SectionReport {
        section: "attention",
        findings,
    }
}

/// Classify one embedding route's status row into a doctor finding.
///
/// The visual lane gets its own rule: a `visual:<kind>` route whose only
/// problem is `not_configured` is OPT-IN ABSENCE (design principle 4:
/// visual embedding ships unconfigured until the visual eval exists), so
/// it reports as `info` with the enabling stanza, never as a failure.
/// Everything else follows failure semantics: credentials and hard route
/// errors are `action` (a fixing command exists), queue pressure and
/// permanent drops are `warn`.
pub(crate) fn classify_embed_route(
    route: &str,
    status: &crate::embed::queue::RouteStatus,
) -> Finding {
    let visual_kind = route.strip_prefix("visual:");
    // Coverage-seeded visual rows: the corpus contains chunks of this kind
    // but no queue worker or route metadata exists (provider/model empty).
    // Same opt-in story as the not_configured error row, reached without a
    // failed enqueue in this process (e.g. right after a restart).
    if let Some(kind) = visual_kind {
        if status.available
            && status.provider.is_none()
            && status.model.is_none()
            && status.source_count.unwrap_or(0) > 0
        {
            return Finding::info(format!(
                "visual chunk kind `{kind}` has {} source chunk(s) but is not \
                 opted in to embedding (visual retrieval is opt-in per kind)",
                status.source_count.unwrap_or(0)
            ))
            .with_next(format!(
                "add `[embed.routes.visual] {kind} = \"voyage_visual\"` to \
                 embed.toml, then bbox_reembed"
            ));
        }
    }
    if !status.available {
        let reason = status.health_reason.as_deref().unwrap_or("unavailable");
        let detail = status.last_error.as_deref().unwrap_or(reason);
        if let Some(kind) = visual_kind {
            if reason == "not_configured" {
                return Finding::info(format!(
                    "visual chunk kind `{kind}` is not opted in to embedding \
                     (visual retrieval is opt-in per kind)"
                ))
                .with_next(format!(
                    "add `[embed.routes.visual] {kind} = \"voyage_visual\"` to \
                     embed.toml, then bbox_reembed"
                ));
            }
        }
        return match reason {
            "credential_missing" => Finding::action(
                format!("route `{route}` is missing provider credentials: {detail}"),
                format!("set the provider API key env, then bbox_reembed(route=\"{route}\")"),
            ),
            "queue_full" => Finding::warn(format!(
                "route `{route}` queue is full ({} pending, {} bytes)",
                status.queue_depth, status.queue_bytes
            )),
            _ => Finding::action(
                format!("route `{route}` is unavailable: {detail}"),
                format!("fix the route config/provider, then bbox_reembed(route=\"{route}\")"),
            ),
        };
    }
    if status.dropped_count > 0 {
        let detail = status.last_dropped.as_deref().unwrap_or("unknown");
        return Finding::warn(format!(
            "route `{route}` permanently dropped {} item(s) (poison/retry-exhausted); \
             last: {detail}",
            status.dropped_count
        ));
    }
    Finding::ok(format!(
        "route `{route}` ok ({} indexed, {} queued)",
        status.indexed_count, status.queue_depth
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::queue::RouteStatus;

    fn base_status() -> RouteStatus {
        RouteStatus {
            available: true,
            health: "ok".into(),
            health_reason: None,
            provider: Some("voyage".into()),
            model: Some("voyage-code-3".into()),
            query_model: None,
            endpoint_kind: None,
            output_dtype: None,
            compatibility_family: None,
            dim: Some(1024),
            source_count: None,
            indexed_count: 10,
            session_indexed_count: None,
            queue_depth: 0,
            queue_bytes: 0,
            retried_count: 0,
            last_error: None,
            coverage_ratio: None,
            coverage_state: None,
            dropped_count: 0,
            last_dropped: None,
            capped_count: 0,
        }
    }

    #[test]
    fn healthy_route_is_ok() {
        let finding = classify_embed_route("code", &base_status());
        assert_eq!(finding.level, FindingLevel::Ok);
    }

    /// The incident rule: an unconfigured visual kind is opt-in state,
    /// not a failure — `info` with the enabling stanza as next step.
    #[test]
    fn unconfigured_visual_kind_is_info_with_opt_in_stanza() {
        let mut status = base_status();
        status.available = false;
        status.health = "unavailable".into();
        status.health_reason = Some("not_configured".into());
        status.last_error = Some("visual chunk kind `image` has no configured route".into());
        let finding = classify_embed_route("visual:image", &status);
        assert_eq!(finding.level, FindingLevel::Info);
        assert!(finding.message.contains("opt-in"), "{finding:?}");
        assert!(
            finding
                .next
                .as_deref()
                .unwrap_or_default()
                .contains("[embed.routes.visual]"),
            "{finding:?}"
        );
    }

    /// A coverage-seeded visual row (source chunks exist, no route
    /// metadata, no failed enqueue yet) gets the same opt-in `info`.
    #[test]
    fn coverage_seeded_unrouted_visual_row_is_info() {
        let mut status = base_status();
        status.provider = None;
        status.model = None;
        status.source_count = Some(12);
        let finding = classify_embed_route("visual:pdf_figure", &status);
        assert_eq!(finding.level, FindingLevel::Info);
        assert!(
            finding
                .next
                .as_deref()
                .unwrap_or_default()
                .contains("pdf_figure"),
            "{finding:?}"
        );
    }

    /// The same not_configured reason on a TEXT route is a real failure:
    /// text buckets are supposed to be routed.
    #[test]
    fn unconfigured_text_route_is_action() {
        let mut status = base_status();
        status.available = false;
        status.health_reason = Some("not_configured".into());
        status.last_error = Some("embedding route is not configured".into());
        let finding = classify_embed_route("docs", &status);
        assert_eq!(finding.level, FindingLevel::Action);
    }

    #[test]
    fn credential_missing_is_action_with_reembed_next() {
        let mut status = base_status();
        status.available = false;
        status.health_reason = Some("credential_missing".into());
        status.last_error = Some("VOYAGE_API_KEY not set".into());
        let finding = classify_embed_route("knowledge", &status);
        assert_eq!(finding.level, FindingLevel::Action);
        assert!(
            finding
                .next
                .as_deref()
                .unwrap_or_default()
                .contains("bbox_reembed"),
            "{finding:?}"
        );
    }

    #[test]
    fn visual_route_with_credential_failure_is_action_not_info() {
        let mut status = base_status();
        status.available = false;
        status.health_reason = Some("credential_missing".into());
        let finding = classify_embed_route("visual:image", &status);
        assert_eq!(finding.level, FindingLevel::Action);
    }

    #[test]
    fn dropped_items_are_warn() {
        let mut status = base_status();
        status.dropped_count = 3;
        status.last_dropped = Some("project_file:p:f:h:0 (HTTP 400)".into());
        let finding = classify_embed_route("docs", &status);
        assert_eq!(finding.level, FindingLevel::Warn);
    }

    /// End-to-end over a per-test SharedState: every v0 section shows up,
    /// nothing panics on an empty daemon, and the report serializes.
    #[test]
    fn run_produces_all_sections_on_an_empty_test_state() {
        crate::init_system_memory_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let server = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(tmp.path()),
        ));
        let report = run(&server).expect("doctor run");
        let names: Vec<&str> = report.sections.iter().map(|s| s.section).collect();
        assert_eq!(
            names,
            vec![
                "daemon",
                "index",
                "vectors",
                "graph",
                "projects",
                "memories",
                "knowledge",
                "attention"
            ]
        );
        assert!(
            report.sections.iter().all(|s| !s.findings.is_empty()),
            "every section reports at least one finding: {report:?}"
        );
        serde_json::to_string(&report).expect("report serializes");
        // Renders without panicking and leads with the status line.
        assert!(report.render_summary().starts_with("status: "));
    }

    #[test]
    fn report_status_is_worst_finding_and_summary_groups_by_level() {
        let report = DoctorReport::from_sections(vec![
            SectionReport {
                section: "daemon",
                findings: vec![Finding::ok("version 0.1.0")],
            },
            SectionReport {
                section: "vectors",
                findings: vec![
                    Finding::info("visual kind image not opted in"),
                    Finding::action("route docs broken", "bbox_reembed(route=\"docs\")"),
                ],
            },
        ]);
        assert_eq!(report.status, FindingLevel::Action);
        let summary = report.render_summary();
        assert!(summary.starts_with("status: action\n"), "{summary}");
        let action_pos = summary.find("action:").unwrap();
        let info_pos = summary.find("info:").unwrap();
        assert!(action_pos < info_pos, "worst-first grouping: {summary}");
        assert!(summary.contains("next: bbox_reembed"), "{summary}");
        assert!(summary.contains("ok: daemon"), "{summary}");
    }
}
