//! `bbox_doctor` v0: one read-only "what do I need to know right now?"
//! surface (design/operations/config-artifacts/ops-artifact-bundles-and-doctor.md,
//! Phase 5 pulled forward). Aggregates existing health signals in-process
//! and classifies findings; it never mutates stores.
//!
//! v0 ships the substrate-independent sections only: daemon, index,
//! code sources, vectors, graph, projects, checkout access, memories,
//! knowledge, and attention. The
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

/// Operator CLI form of an ops-surface tool call. Doctor's next steps name
/// operator tools, which agent-facing MCP surfaces do not serve.
fn ops_call(tool: &str, args: &str) -> String {
    format!("bro mcp call {tool} '{args}' --surface ops")
}

fn publisher_status_call(project: &str) -> String {
    ops_call(
        "bbox_project_publisher_status",
        &format!("{{\"project_id\":\"{project}\"}}"),
    )
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
    /// Complete path-free checkout observation projection for programmatic
    /// consumers. The compact summary remains finding-oriented, while JSON
    /// exposes every closed operation kind and bounded counter dimension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) checkout_access: Option<bbox_indexing::checkout_access::CheckoutAccessHealth>,
    /// Complete path-free knowledge transport overlap evidence. The section
    /// below classifies the operator-relevant failures; JSON retains the
    /// bounded per-project counters and latest shadow comparisons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) knowledge_transport: Option<
        bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationSnapshotV1,
    >,
}

impl DoctorReport {
    pub(crate) fn from_sections(sections: Vec<SectionReport>) -> Self {
        let status = sections
            .iter()
            .map(SectionReport::worst)
            .max()
            .unwrap_or(FindingLevel::Ok);
        Self {
            status,
            sections,
            checkout_access: None,
            knowledge_transport: None,
        }
    }

    fn with_checkout_access(
        mut self,
        health: bbox_indexing::checkout_access::CheckoutAccessHealth,
    ) -> Self {
        self.checkout_access = Some(health);
        self
    }

    fn with_knowledge_transport(
        mut self,
        observations: bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationSnapshotV1,
    ) -> Self {
        self.knowledge_transport = Some(observations);
        self
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
    let mut sections = vec![
        daemon_section(state),
        index_section(state),
        code_sources_section(state),
        vectors_section(state),
        snapshots_section(state),
        projects_section(state),
    ];
    let checkout_access = state.checkout_access_observations.health();
    sections.push(checkout_access_section(&checkout_access));
    // Knowledge transport is a catalog authority surface. Keeping it out of
    // bridge reports preserves the frozen bridge-parity response and avoids
    // presenting cutover controls where no catalog marker can exist.
    let knowledge_transport = state
        .project_authority
        .catalog_store()
        .is_some()
        .then(|| state.knowledge_transport_observations.snapshot());
    if let Some(observations) = &knowledge_transport {
        sections.push(knowledge_transport_section(
            state,
            &checkout_access,
            observations,
        ));
    }
    // Catalog-only project health (plan section 8, P5-G). Each section is
    // observational: it reports what the catalog, the accepted pointer, and
    // the runtime's published observations already say, and never counts an
    // operation nobody attempted.
    let project_statuses = catalog_project_statuses(state);
    if let Some(statuses) = project_statuses.as_ref() {
        sections.extend([
            accepted_publication_section(statuses),
            publisher_binding_section(statuses),
            overlay_baseline_section(statuses),
            attachment_capability_section(statuses),
            artifact_watcher_section(statuses),
        ]);
    }
    sections.extend([memories_section(state), attention_section(state)]);
    let mut report = DoctorReport::from_sections(sections).with_checkout_access(checkout_access);
    if let Some(observations) = knowledge_transport {
        report = report.with_knowledge_transport(observations);
    }
    Ok(report)
}

/// Applicable section vocabulary in report order. Deliberately cheap: this
/// is the pre-scan validation surface, so it only probes authority mode and
/// never invokes a section producer.
pub(crate) fn section_names(state: &crate::server::state::SharedState) -> Vec<&'static str> {
    let catalog = state.project_authority.catalog_store().is_some();
    let mut names = vec![
        "daemon",
        "index",
        "code_sources",
        "vectors",
        "snapshots",
        "projects",
        "checkout_access",
    ];
    if catalog {
        names.push("knowledge_transport");
    }
    if catalog {
        names.extend([
            "accepted_publication",
            "publisher_binding",
            "overlay_baseline",
            "attachment_capability",
            "artifact_watcher",
        ]);
    }
    names.extend(["memories", "attention"]);
    names
}

/// Collect exactly one section through its existing independent producer.
/// A14: a requested section must not pay for (or depend on) collection of
/// the unrelated sections. The section name is validated against the
/// applicable vocabulary BEFORE any collection runs.
pub(crate) fn run_section(
    server: &crate::server::BlackboxServer,
    section: &str,
) -> anyhow::Result<DoctorReport> {
    let state = &server.state;
    if !section_names(state).contains(&section) {
        anyhow::bail!("Unknown section `{section}`; omit section to discover available names");
    }
    if section == "checkout_access" {
        let checkout_access = state.checkout_access_observations.health();
        let section_report = checkout_access_section(&checkout_access);
        return Ok(
            DoctorReport::from_sections(vec![section_report]).with_checkout_access(checkout_access)
        );
    }
    if section == "knowledge_transport" {
        let checkout_access = state.checkout_access_observations.health();
        let observations = state.knowledge_transport_observations.snapshot();
        let section_report = knowledge_transport_section(state, &checkout_access, &observations);
        return Ok(DoctorReport::from_sections(vec![section_report])
            .with_knowledge_transport(observations));
    }
    let section_report = match section {
        "daemon" => daemon_section(state),
        "index" => index_section(state),
        "code_sources" => code_sources_section(state),
        "vectors" => vectors_section(state),
        "snapshots" => snapshots_section(state),
        "projects" => projects_section(state),
        "accepted_publication"
        | "publisher_binding"
        | "overlay_baseline"
        | "attachment_capability"
        | "artifact_watcher" => {
            let statuses = catalog_project_statuses(state).unwrap_or_default();
            match section {
                "accepted_publication" => accepted_publication_section(&statuses),
                "publisher_binding" => publisher_binding_section(&statuses),
                "overlay_baseline" => overlay_baseline_section(&statuses),
                "attachment_capability" => attachment_capability_section(&statuses),
                _ => artifact_watcher_section(&statuses),
            }
        }
        "memories" => memories_section(state),
        "attention" => attention_section(state),
        _ => unreachable!("section vocabulary was validated above"),
    };
    Ok(DoctorReport::from_sections(vec![section_report]))
}

// Collect every finding before response projection. MCP summaries page these
// records; exact reads must never inherit a producer-side preview cutoff.

/// Project every catalog project's runtime status once, for the sections
/// below to read. `None` in bridge mode, where these sections do not apply
/// and are omitted from the report entirely rather than rendered empty.
fn catalog_project_statuses(
    state: &crate::server::state::SharedState,
) -> Option<Vec<crate::server::state::ProjectRuntimeStatus>> {
    state.project_authority.catalog_store()?;
    let snapshot = state.records_provider.records_snapshot();
    Some(
        snapshot
            .corpus_project_ids
            .iter()
            .filter_map(|project_id| state.project_runtime_status(project_id))
            .collect(),
    )
}

/// Accepted-publication state per project, plus the two states that read
/// fine but refuse mutation: Prior fallback and the scope-migration bridge.
fn accepted_publication_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut current = 0;
    for status in statuses {
        // An unreadable catalog pair is reported FIRST, then the accepted
        // state is reported BESIDE it. Both facts, not one: the catalog and
        // the accepted pointer are separate durable stores that degrade
        // separately, so "catalog unreadable" says nothing about whether
        // published content is still serving, and an operator who sees only
        // the first has no way to find out mid-poisoning
        // (bbox_project_publisher_status needs a catalog snapshot itself).
        if status.catalog_authority == "unavailable" {
            let project = &status.project_id;
            findings.push(Finding::action(
                format!(
                    "project {project} could not be read from the catalog pair; \
                     its catalog-derived status is unavailable, not denied"
                ),
                ops_call("bbox_doctor", "{}"),
            ));
            findings.push(match status.accepted.state {
                "current" if status.accepted.serves_published_content => Finding::info(format!(
                    "project {project} accepted publication is verified independently \
                         of the catalog and is CURRENT; published knowledge and gaps keep \
                         serving while the catalog pair is unreadable"
                )),
                "prior" => Finding::action(
                    format!(
                        "project {project} accepted publication is verified independently \
                         of the catalog and fell back to its PRIOR generation; reads \
                         continue and every mutation refuses until repair"
                    ),
                    publisher_status_call(project),
                ),
                "missing" => Finding::info(format!(
                    "project {project} has no accepted publication pointer; that is \
                     independent of the unreadable catalog pair"
                )),
                "corrupt" => Finding::action(
                    format!(
                        "project {project} accepted publication is CORRUPT independently \
                         of the unreadable catalog pair; published reads are unavailable \
                         for it"
                    ),
                    publisher_status_call(project),
                ),
                other => Finding::warn(format!(
                    "project {project} accepted publication state is {other} and could \
                     not be evaluated further while the catalog pair is unreadable"
                )),
            });
            continue;
        }
        let notable = match status.accepted.state {
            "current" => status.accepted.scope_agreement == "refresh_required",
            _ => true,
        };
        if !notable {
            current += 1;
            continue;
        }
        let project = &status.project_id;
        findings.push(match status.accepted.state {
            // Serving old accepted truth under its old scope until a
            // scope move at the new scope clears the bridge (plan 4.9).
            _ if status.accepted.scope_agreement == "refresh_required" => Finding::action(
                format!(
                    "project {project} serves accepted content at a scope the catalog has since \
                     migrated; a scope_move to a candidate at the current scope clears the bridge"
                ),
                ops_call("bbox_project_publisher_advance", "<json>"),
            ),
            // Reads continue off the prior arm; every mutation refuses.
            "prior" => Finding::action(
                format!(
                    "project {project} fell back to its PRIOR accepted generation; reads continue \
                     and establish, bind, and advance all refuse until repair"
                ),
                publisher_status_call(project),
            ),
            // A connector or legacy-local project with no attachment can
            // never accept a candidate: no published scope, no producer.
            "missing" if !has_publication_lane(status) => Finding::info(format!(
                "project {project} has no accepted publication lane (no published scope and \
                 no attached checkout)"
            )),
            "missing" => Finding::info(format!(
                "project {project} has no accepted publication pointer; its first valid \
                 candidate from the owning producer establishes one"
            )),
            "corrupt" => Finding::blocked(format!(
                "project {project} has an accepted pointer whose current and prior arms both \
                 failed verification{}",
                status
                    .accepted
                    .diagnostic
                    .as_deref()
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default()
            )),
            "unavailable" => Finding::warn(format!(
                "project {project} accepted status could not be read from the runtime"
            )),
            other => Finding::info(format!("project {project} accepted state {other}")),
        });
    }

    if findings.is_empty() && current > 0 {
        findings.push(Finding::ok(format!(
            "{current} catalog project(s) serve their current accepted generation"
        )));
    }
    SectionReport {
        section: "accepted_publication",
        findings,
    }
}

/// Whether a project can ever hold an accepted publication: it needs a
/// published catalog scope or an attached checkout.
fn has_publication_lane(status: &crate::server::state::ProjectRuntimeStatus) -> bool {
    status.catalog_scope.is_some()
        || status
            .attachments
            .iter()
            .any(|attachment| attachment.status == "attached")
}

/// Which attachment each pointer names and whether an advance can run.
fn publisher_binding_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut healthy = 0;
    for status in statuses {
        let project = &status.project_id;
        let finding = match status.binding.status {
            // D-033 item 1 made observable: detach does not take the
            // publication lock, so a pointer can outlive its attachment.
            "detached" => Some(Finding::action(
                format!(
                    "project {project} pointer names a DETACHED attachment; published reads \
                     continue and advance is unavailable until an explicit bind repairs it"
                ),
                ops_call("bbox_project_publisher_bind", "<json>"),
            )),
            "unknown_attachment" => Some(Finding::warn(format!(
                "project {project} pointer names an attachment the catalog no longer carries"
            ))),
            "unbound" => None,
            _ if !status.accepted.advance_available
                && status.accepted.state != "missing"
                && status.accepted.state != "unavailable" =>
            {
                Some(Finding::info(format!(
                    "project {project} advance is unavailable from accepted state \
                     ({})",
                    status.accepted.state
                )))
            }
            _ => {
                healthy += 1;
                None
            }
        };
        if let Some(finding) = finding {
            findings.push(finding);
        }
    }

    if findings.is_empty() && healthy > 0 {
        findings.push(Finding::ok(format!(
            "{healthy} catalog pointer binding(s) name an attached attachment"
        )));
    }
    SectionReport {
        section: "publisher_binding",
        findings,
    }
}

/// Last published overlay outcome per checkout, per lane.
fn overlay_baseline_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut fresh = 0;
    for status in statuses {
        for overlay in &status.overlays {
            if overlay.outcome == "fresh" {
                fresh += 1;
                continue;
            }
            findings.push(Finding::warn(format!(
                "project {} checkout {} {} overlay unavailable{}",
                status.project_id,
                overlay.checkout_id,
                overlay.lane,
                overlay
                    .diagnostics
                    .first()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default(),
            )));
        }
    }

    if findings.is_empty() && fresh > 0 {
        findings.push(Finding::ok(format!("{fresh} checkout overlay(s) fresh")));
    }
    SectionReport {
        section: "overlay_baseline",
        findings,
    }
}

/// Capability availability by attachment, straight from the catalog bits.
///
/// An attachment with no recorded capability is the actionable case: it is
/// attached but every lane degrades. Nothing here is a denial count; no
/// operation was attempted (plan 4.17).
fn attachment_capability_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut attached = 0;
    let mut remote_only = 0;
    for status in statuses {
        let active = status
            .attachments
            .iter()
            .filter(|attachment| attachment.status == "attached")
            .collect::<Vec<_>>();
        if active.is_empty() {
            remote_only += 1;
            continue;
        }
        for attachment in active {
            attached += 1;
            if !attachment.available.is_empty() {
                continue;
            }
            findings.push(Finding::warn(format!(
                "project {} attachment {} records no capabilities; every checkout-backed lane \
                 degrades for it",
                status.project_id, attachment.attachment_id
            )));
        }
    }

    if remote_only > 0 {
        findings.push(Finding::info(format!(
            "{remote_only} catalog project(s) are remote-only; published reads serve and every \
             checkout-backed lane reports unavailable"
        )));
    }
    if findings.is_empty() && attached > 0 {
        findings.push(Finding::ok(format!(
            "{attached} active attachment(s) record at least one capability"
        )));
    }
    SectionReport {
        section: "attachment_capability",
        findings,
    }
}

/// Watcher registration state per project.
fn artifact_watcher_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut registered = 0;
    if statuses
        .iter()
        .all(|status| !status.watcher.watcher_running)
    {
        return SectionReport {
            section: "artifact_watcher",
            findings: vec![Finding::info(
                "no artifact watcher runs in this process; durable artifact metadata is                  unaffected and filesystem discovery is off",
            )],
        };
    }
    for status in statuses {
        registered += status.watcher.registered_attachments.len();
    }
    if registered == 0 {
        // The cage signature: a watcher runs, but every capable attachment
        // lives on another host, so nothing local can register. One
        // process-level note instead of a warning per project. A partial
        // registration set still reports per project below.
        return SectionReport {
            section: "artifact_watcher",
            findings: vec![Finding::info(
                "artifact watcher has zero local registrations; every capable attachment lives on another host",
            )],
        };
    }
    for status in statuses {
        if status.watcher.capable_but_unregistered.is_empty() {
            continue;
        }
        findings.push(Finding::warn(format!(
            "project {} has {} attachment(s) recording artifact_watching with no live watcher \
             registration",
            status.project_id,
            status.watcher.capable_but_unregistered.len()
        )));
    }

    if findings.is_empty() {
        findings.push(Finding::ok(format!(
            "{registered} attachment watcher registration(s) active"
        )));
    }
    SectionReport {
        section: "artifact_watcher",
        findings,
    }
}

fn checkout_access_section(
    health: &bbox_indexing::checkout_access::CheckoutAccessHealth,
) -> SectionReport {
    let mut findings = Vec::new();
    if health.sequence == 0 {
        findings.push(Finding::info(
            "checkout access broker has no observations yet",
        ));
    } else {
        for operation in health
            .operations
            .iter()
            .filter(|operation| operation.granted > 0 || operation.denied > 0)
        {
            let last_success = operation
                .last_success_unix_secs
                .map(|value| value.to_string())
                .unwrap_or_else(|| "never".into());
            findings.push(Finding::info(format!(
                "{}: {} granted, {} denied, last success {}",
                operation.kind.as_str(),
                operation.granted,
                operation.denied,
                last_success,
            )));
        }
        if !health.active_compatibility_lanes.is_empty() {
            let lanes = health
                .active_compatibility_lanes
                .iter()
                .map(|lane| lane.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(Finding::info(format!(
                "active checkout compatibility lanes: {lanes}"
            )));
        }
    }
    SectionReport {
        section: "checkout_access",
        findings,
    }
}

/// Strict knowledge transport authority and overlap evidence. Marker rows
/// are monotonic: every non-current classification is a remote degradation,
/// never permission to reopen the local adapter.
fn knowledge_transport_section(
    state: &crate::server::state::SharedState,
    checkout_health: &bbox_indexing::checkout_access::CheckoutAccessHealth,
    observations: &bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationSnapshotV1,
) -> SectionReport {
    use bbox_indexing::checkout_access::CheckoutAccessOutcome;
    use bbox_indexing::knowledge_transport_cutover::KnowledgeTransportRuntimeCoverageV1;
    use bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1;

    let mut findings = Vec::new();
    let Some(marker) = state.knowledge_transport_cutover.marker() else {
        findings.push(if observations.sequence == 0 {
            Finding::info("knowledge transport has no observations or strict cutover rows yet")
        } else {
            Finding::info(format!(
                "knowledge transport has {} overlap observation(s) and no strict cutover rows",
                observations.sequence
            ))
        });
        return SectionReport {
            section: "knowledge_transport",
            findings,
        };
    };

    let catalog = state
        .project_authority
        .catalog_store()
        .ok_or_else(|| "catalog store unavailable".to_string())
        .and_then(|store| store.snapshot().map_err(|error| format!("{error:#}")));
    let assignments = state
        .code_sources
        .producer_auth()
        .repo_assignment_producers();
    let covered_ids = marker
        .rows
        .iter()
        .map(|row| row.project_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let parity_workspaces = marker
        .rows
        .iter()
        .flat_map(|row| {
            row.parity_workspace_ids
                .iter()
                .map(move |workspace_id| (row.project_id.as_str(), workspace_id.as_str()))
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut current = 0usize;

    match catalog {
        Err(error) => findings.push(Finding::blocked(format!(
            "knowledge transport has {} strict row(s), but live catalog classification failed: {error}",
            marker.rows.len()
        ))),
        Ok(catalog) => {
            for row in &marker.rows {
                let accepted = state
                    .accepted_publications
                    .as_ref()
                    .and_then(|runtime| runtime.load_verified(&row.project_id).ok());
                let coverage = state.knowledge_transport_cutover.classify_project(
                    catalog.catalog(),
                    &assignments,
                    &row.project_id,
                    accepted.as_ref(),
                );
                if coverage == KnowledgeTransportRuntimeCoverageV1::Current {
                    current += 1;
                } else {
                    findings.push(Finding::action(
                        format!(
                            "project {} remains transport-governed but is {}; local fallback stays closed",
                            row.project_id,
                            serde_json::to_value(coverage)
                                .ok()
                                .and_then(|value| value.as_str().map(str::to_owned))
                                .unwrap_or_else(|| "pending_recutover".into())
                        ),
                        "blackbox project-catalog knowledge-transport-cutover --preflight",
                    ));
                }

                for baseline in &row.capability_baselines {
                    let count = |outcome| {
                        checkout_health
                            .target_counters
                            .iter()
                            .filter(|counter| {
                                counter.project_id == row.project_id.as_str()
                                    && counter.kind == baseline.capability
                                    && counter.outcome == outcome
                            })
                            .map(|counter| counter.count)
                            .sum::<u64>()
                    };
                    let granted = count(CheckoutAccessOutcome::Granted);
                    let denied = count(CheckoutAccessOutcome::Denied);
                    if granted != baseline.granted || denied != baseline.denied {
                        findings.push(Finding::blocked(format!(
                            "project {} recorded a post-cutover {} checkout observation (baseline {}/{}, current {}/{})",
                            row.project_id,
                            baseline.capability.as_str(),
                            baseline.granted,
                            baseline.denied,
                            granted,
                            denied
                        )));
                    }
                }
            }
        }
    }

    let mismatch_count = observations
        .comparisons
        .iter()
        .filter(|comparison| {
            comparison
                .workspace_id
                .as_deref()
                .is_some_and(|workspace_id| {
                    parity_workspaces.contains(&(comparison.project_id.as_str(), workspace_id))
                })
        })
        .filter(|comparison| !comparison.equal)
        .count();
    if mismatch_count > 0 {
        findings.push(Finding::action(
            format!(
                "{mismatch_count} latest covered-project knowledge transport shadow comparison(s) mismatch"
            ),
            "blackbox project-catalog knowledge-transport-cutover --preflight",
        ));
    }
    let remote_count = observations
        .counters
        .iter()
        .filter(|counter| {
            covered_ids.contains(counter.project_id.as_str())
                && counter.outcome == KnowledgeTransportOutcomeV1::Remote
        })
        .map(|counter| counter.count)
        .sum::<u64>();
    findings.extend(degraded_transport_finding(
        observations.counters.iter().filter(|counter| {
            covered_ids.contains(counter.project_id.as_str())
                && counter.outcome == KnowledgeTransportOutcomeV1::Degraded
        }),
        unix_now_secs(),
    ));

    if current > 0 {
        findings.push(Finding::ok(format!(
            "{current} strict knowledge transport row(s) are current; {remote_count} remote operation(s) observed"
        )));
    }
    if findings.is_empty() {
        findings.push(Finding::ok(
            "strict knowledge transport marker contains no rows",
        ));
    }
    SectionReport {
        section: "knowledge_transport",
        findings,
    }
}

/// A degraded knowledge transport counter that advanced within this window
/// is current degradation; older counters are lifetime history.
const KNOWLEDGE_TRANSPORT_DEGRADED_RECENT_SECS: u64 = 24 * 3_600;

/// Classify covered degraded knowledge transport counters. Counters are
/// lifetime totals, so only a counter that advanced within the recent window
/// warns; the lifetime total is reported as info otherwise.
fn degraded_transport_finding<'a>(
    degraded: impl Iterator<
        Item = &'a bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationCounterV1,
    >,
    now_unix_secs: u64,
) -> Option<Finding> {
    let mut lifetime = 0u64;
    let mut latest: Option<
        &bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationCounterV1,
    > = None;
    let mut recent_counters = 0usize;
    for counter in degraded {
        lifetime += counter.count;
        if now_unix_secs.saturating_sub(counter.last_unix_secs)
            <= KNOWLEDGE_TRANSPORT_DEGRADED_RECENT_SECS
        {
            recent_counters += 1;
        }
        if latest.is_none_or(|current| counter.last_unix_secs > current.last_unix_secs) {
            latest = Some(counter);
        }
    }
    let latest = latest?;
    let latest_age_hours = now_unix_secs.saturating_sub(latest.last_unix_secs) / 3_600;
    let latest_label = format!(
        "latest {} {} {latest_age_hours}h ago",
        latest.project_id,
        serde_json::to_value(latest.operation)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "operation".into()),
    );
    Some(if recent_counters > 0 {
        Finding::warn(format!(
            "covered knowledge transport recorded degraded operations in the last 24h across {recent_counters} project operation(s) ({latest_label}; {lifetime} lifetime); local fallback remained closed"
        ))
    } else {
        Finding::info(format!(
            "covered knowledge transport recorded no degraded operation in the last 24h ({lifetime} lifetime, {latest_label})"
        ))
    })
}

/// Git-history activations the background lane dead-lettered because the
/// producer grant table refuses their repository. The dead letter is the
/// durable record; it disappears when the grant resolves, the source stops
/// being a redrive candidate, or an operator drops it.
fn history_activation_deadletter_findings(
    state: &crate::server::state::SharedState,
) -> Vec<Finding> {
    let deadletters = match state.git_sources.store().list_activation_deadletters() {
        Ok(deadletters) => deadletters,
        Err(error) => {
            return vec![Finding::warn(format!(
                "Git-history activation dead letters are unreadable: {error:#}"
            ))];
        }
    };
    deadletters
        .into_iter()
        .map(|deadletter| {
            let repo_history = deadletter.repo_history_id.as_str();
            let message = format!(
                "repository history `{repo_history}` typed activation is dead-lettered ({}): {}; source generation `{}` is not redriven until the catalog or producer grant changes",
                deadletter.error_code,
                crate::server::history_activation::deadletter_diagnostic(&deadletter.error_code),
                deadletter.source_generation_id,
            );
            let next = if deadletter.error_code == "repo_history_not_found" {
                format!(
                    "bind a catalog project to this repository history, or retire its ready pointer with `blackbox git-history activations-drop --repo-history {repo_history} --retire-ready-pointer`"
                )
            } else {
                "repair the producer grant for this repository; the activation lane resumes automatically once the grant resolves".to_string()
            };
            Finding::action(message, next)
        })
        .collect()
}

/// Catalog projects with no code-source lane by design: no attached
/// checkout and no code-source producer assignment. Empty when the catalog is
/// unavailable, so nothing is downgraded on missing evidence.
fn projects_without_code_source_lane(
    state: &crate::server::state::SharedState,
) -> std::collections::BTreeSet<String> {
    use bbox_corpus_core::project_catalog::AttachmentStatus;

    let Some(snapshot) = state
        .project_authority
        .catalog_store()
        .and_then(|store| store.snapshot().ok())
    else {
        return std::collections::BTreeSet::new();
    };
    let assigned = state
        .code_sources
        .producer_auth()
        .assignment_map()
        .into_values()
        .map(|(project_id, _producer)| project_id)
        .collect::<std::collections::BTreeSet<_>>();
    let attached = snapshot
        .attachments()
        .attachments
        .values()
        .filter(|attachment| attachment.status == AttachmentStatus::Attached)
        .map(|attachment| attachment.project_id.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    snapshot
        .catalog()
        .projects
        .keys()
        .map(|project_id| project_id.as_str().to_string())
        .filter(|project_id| !assigned.contains(project_id) && !attached.contains(project_id))
        .collect()
}

/// `source_unavailable` is a fault only for a project that has a code-source
/// lane. A project with neither an attachment nor a producer assignment has
/// no source by design.
fn source_unavailable_finding(project_id: &str, diagnostic: &str, has_lane: bool) -> Finding {
    if !has_lane {
        return Finding::info(format!(
            "project `{project_id}` has no code-source lane (no attached checkout and no code-source producer assignment); code search does not cover it"
        ));
    }
    Finding::warn(format!(
        "project `{project_id}` has no usable code source this pass: {diagnostic}"
    ))
    .with_next("attach a checkout, or publish a collected generation, to give the project a source")
}

fn code_sources_section(state: &crate::server::state::SharedState) -> SectionReport {
    let store = state.code_sources.store();
    let mut findings = Vec::new();
    let without_lane = projects_without_code_source_lane(state);
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
                    // P3-C planning states. Typed here rather than falling
                    // through to the generic warn arm so an operator sees the
                    // ONE action each admits, and so `empty_root_refused`
                    // reads as the deliberate refusal it is rather than as an
                    // unexplained warning.
                    "source_unavailable" => source_unavailable_finding(
                        &record.project_id,
                        &record.diagnostic,
                        !without_lane.contains(&record.project_id),
                    ),
                    "empty_root_refused" => Finding::action(
                        format!(
                            "project `{}` local scan returned zero entries and the purge was refused: {}",
                            record.project_id, record.diagnostic
                        ),
                        format!(
                            "restore the checkout, or acknowledge the empty root with `{}`",
                            ops_call(
                                "bbox_reindex",
                                &format!("{{\"accept_empty_projects\":[\"{}\"]}}", record.project_id),
                            )
                        ),
                    ),
                    // P3-F history states. `unavailable_no_attachment` is the
                    // remote-only STEADY STATE, so it is informational: the
                    // commit documents stay readable and there is nothing to
                    // repair. It replaces the `git_history_unavailable` this
                    // case used to record, which read as a Git fault forever.
                    bbox_indexing::index::history_health::HISTORY_UNAVAILABLE_NO_ATTACHMENT_CODE => {
                        Finding::info(format!(
                            "project `{}` repository history cannot be refreshed: {}",
                            record.project_id, record.diagnostic
                        ))
                    }
                    bbox_indexing::index::history_health::HISTORY_REFRESH_FAILED_CODE => {
                        Finding::action(
                            format!(
                                "project `{}` last repository-history refresh failed: {}",
                                record.project_id, record.diagnostic
                            ),
                            "inspect daemon logs for the walk failure, then publish the checkout again to retry",
                        )
                    }
                    bbox_indexing::index::history_health::HISTORY_TRANSPORT_ACTIVATION_FAILED_CODE => {
                        Finding::action(
                            format!(
                                "project `{}` typed repository-history activation has not converged: {}",
                                record.project_id, record.diagnostic
                            ),
                            "repair the reported producer source, catalog grant, or publication receipt; the background activation lane retries automatically",
                        )
                    }
                    "git_history_unavailable" => Finding::warn(format!(
                        "project `{}` Git current-file overlay is unavailable: {}",
                        record.project_id, record.diagnostic
                    ))
                    .with_next(
                        "restore Git access for the project's attachment; the code generation stays active and searchable",
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
    findings.extend(history_activation_deadletter_findings(state));

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let stale_hours = state.config.read().code_collection.stale_warning_hours;
    match store.activation_records_mixed() {
        Ok(activations) => {
            for activation in activations {
                let generation = match store.find_generation_mixed(activation.generation_id()) {
                    Ok(generation) => generation,
                    Err(error) => {
                        findings.push(Finding::blocked(format!(
                            "project `{}` active collected generation is unreadable: {error:#}",
                            activation.project_id()
                        )));
                        continue;
                    }
                };
                let age_hours = now
                    .saturating_sub(activation.activated_unix_secs())
                    .checked_div(3_600)
                    .unwrap_or_default();
                if age_hours >= stale_hours {
                    findings.push(Finding::warn(format!(
                        "project `{}` collected generation is {} hours old",
                        activation.project_id(),
                        age_hours
                    )));
                } else if generation.state() == bbox_code_source::GenerationState::Active {
                    findings.push(Finding::ok(format!(
                        "project `{}` collected generation active ({} files, {} bytes, age {}h)",
                        activation.project_id(),
                        generation.descriptor().file_count,
                        generation.descriptor().logical_bytes,
                        age_hours
                    )));
                }
                if let Some(project) = state
                    .records_provider
                    .records_snapshot()
                    .records
                    .iter()
                    .cloned()
                    .find(|project| project.project_id == activation.project_id())
                {
                    use bbox_indexing::checkout_access::{
                        CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
                        CheckoutAccessSourceLane, CheckoutAttachmentSelector,
                    };
                    let request = |kind, expected_scope| CheckoutAccessRequest {
                        project_id: project.project_id.clone(),
                        attachment: CheckoutAttachmentSelector::Selected,
                        expected_scope,
                        kind,
                        intent: CheckoutAccessIntent::Read,
                        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
                    };
                    let expected_scope = if let Some(catalog_store) =
                        state.project_authority.catalog_store()
                    {
                        let project_id =
                            bbox_corpus_core::project_catalog::ProjectId::parse(
                                project.project_id.clone(),
                            )
                            .map_err(|_| {
                                bbox_indexing::checkout_access::CheckoutAccessError::new(
                                    bbox_indexing::checkout_access::CheckoutAccessErrorCode::ScopeMismatch,
                                    "catalog project id is invalid",
                                )
                            });
                        project_id.and_then(|project_id| {
                            let snapshot = catalog_store.snapshot().map_err(|error| {
                                bbox_indexing::checkout_access::CheckoutAccessError::new(
                                    bbox_indexing::checkout_access::CheckoutAccessErrorCode::ScopeMismatch,
                                    format!("catalog scope is unavailable: {error}"),
                                )
                            })?;
                            snapshot
                                .catalog()
                                .projects
                                .get(&project_id)
                                .map(|project| match &project.scope {
                                    bbox_corpus_core::project_catalog::ProjectScope::Published(
                                        scope,
                                    ) => Some(scope.clone()),
                                    bbox_corpus_core::project_catalog::ProjectScope::LegacyLocal
                                    | bbox_corpus_core::project_catalog::ProjectScope::Connector(
                                        _,
                                    ) => None,
                                })
                                .ok_or_else(|| {
                                    bbox_indexing::checkout_access::CheckoutAccessError::new(
                                        bbox_indexing::checkout_access::CheckoutAccessErrorCode::ScopeMismatch,
                                        "catalog project disappeared",
                                    )
                                })
                        })
                    } else {
                        state
                            .checkout_access
                            .acquire(request(CheckoutAccessKind::PublisherConfigTreeRead, None))
                            .map(|scope| scope.published_scope().cloned())
                    };
                    let git = expected_scope.and_then(|expected_scope| {
                        state
                            .checkout_access
                            .acquire(request(CheckoutAccessKind::GitHistory, expected_scope))
                    });
                    let git = match git {
                        Ok(git) => git,
                        Err(error) => {
                            findings.push(Finding::warn(format!(
                                "project `{}` Git-history freshness unavailable ({})",
                                activation.project_id(),
                                error.code.as_str()
                            )));
                            continue;
                        }
                    };
                    let local_head = bbox_corpus_core::git::current_head(git.checkout_root());
                    if let Err(error) = state.checkout_access.revalidate(&git) {
                        findings.push(Finding::warn(format!(
                            "project `{}` Git-history freshness unavailable ({})",
                            activation.project_id(),
                            error.code.as_str()
                        )));
                        continue;
                    }
                    if local_head.as_deref() != Some(generation.descriptor().head_commit.as_str()) {
                        findings.push(Finding::warn(format!(
                            "project `{}` local Git-history HEAD differs from collected current files",
                            activation.project_id()
                        )));
                    }
                }
            }
        }
        Err(error) => findings.push(Finding::blocked(format!(
            "code-source activation records are unreadable: {error:#}"
        ))),
    }
    findings.extend(repo_history_findings(state));
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

/// Repo-history health, rendered beside the code-source findings.
///
/// Catalog mode only: the model is derived from repo-history records and the
/// attachment ladder, neither of which the bridge arm has. Every derivation
/// input is read-only and durable, so this stays inside doctor's no-mutation
/// contract; in particular it observes no checkout head (that would need a
/// lease), which is why the derivation declines to claim `current` here and
/// reports `lagging` with an explicit "not compared" diagnostic instead of
/// guessing.
fn repo_history_findings(state: &crate::server::state::SharedState) -> Vec<Finding> {
    use bbox_indexing::index::history_health::{
        HISTORY_REFRESH_FAILED_CODE, HISTORY_TRANSPORT_ACTIVATION_FAILED_CODE,
        HistoryHealthInputsV1, HistoryHealthStateV1, derive_history_health,
    };

    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Vec::new();
    };
    let Ok(pinned) = catalog_store.snapshot() else {
        return vec![Finding::blocked(
            "the project catalog is unreadable, so repository-history health cannot be derived",
        )];
    };
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &state.idx.read().reindex_config().projects_path,
    );
    let overlays =
        bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir).unwrap_or_default();
    let failed_refreshes = state
        .code_sources
        .store()
        .health_records()
        .unwrap_or_default()
        .into_iter()
        .filter(|record| {
            matches!(
                record.code.as_str(),
                HISTORY_REFRESH_FAILED_CODE | HISTORY_TRANSPORT_ACTIVATION_FAILED_CODE
            )
        })
        .filter_map(|record| {
            // The durable record is per PROJECT; the health model is per
            // REPOSITORY. Map through the catalog rather than assuming the
            // two ids are interchangeable.
            let project_id =
                bbox_corpus_core::project_catalog::ProjectId::parse(record.project_id).ok()?;
            pinned
                .catalog()
                .projects
                .get(&project_id)?
                .repo_history
                .as_ref()
                .map(|id| id.as_str().to_string())
        })
        .collect();
    let mut findings = history_gc_findings(state, pinned.catalog(), &overlays);
    let inputs = HistoryHealthInputsV1 {
        overlays,
        failed_refreshes,
        ..Default::default()
    };
    findings.extend(
        derive_history_health(pinned.catalog(), pinned.attachments(), &inputs)
        .into_iter()
        .map(|record| {
            let members = record.member_project_ids.len();
            let headline = format!(
                "repository history `{}` (namespace `{}`, {members} project(s)) is {}: {}",
                record.repo_history_id,
                record.commit_namespace,
                record.state.as_str(),
                record.diagnostic
            );
            match record.state {
                HistoryHealthStateV1::Current => Finding::ok(headline),
                HistoryHealthStateV1::Lagging
                | HistoryHealthStateV1::UnavailableNoAttachment => Finding::info(headline),
                HistoryHealthStateV1::InvalidScope => Finding::action(
                    headline,
                    "re-validate or replace the attachment so its proved repository matches the project's published scope",
                ),
                HistoryHealthStateV1::FailedLastRefresh => Finding::action(
                    headline,
                    "inspect daemon logs for the walk failure, then publish a member project's checkout again to retry",
                ),
            }
        }),
    );
    findings
}

/// Whether history GC is currently enabled, and why not when it is off
/// (Phase 3 plan section 10 item 4).
///
/// The mismatch arm is deliberately an `action` rather than a `warn`: a
/// disabled sweep is safe but it accumulates retired generations forever, so
/// somebody has to explain the divergence. History READS are unaffected
/// either way, which is why this never escalates to `blocked`.
fn history_gc_findings(
    state: &crate::server::state::SharedState,
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    overlays: &std::collections::BTreeMap<
        String,
        bbox_corpus_core::git_overlay::GitOverlaySelector,
    >,
) -> Vec<Finding> {
    use bbox_indexing::index::history_gc::{
        HistoryGcEnablementV1, build_reference_manifest, evaluate_history_gc,
    };

    let index_path = state.config.read().paths.index_path.clone();
    let Ok(generation_store) =
        bbox_indexing::index::history_generations::HistoryGenerationStore::open_for_index(
            &index_path,
        )
    else {
        return Vec::new();
    };
    let rebuild_manifests = generation_store
        .read_rebuild_manifest()
        .ok()
        .flatten()
        .into_iter()
        .collect::<Vec<_>>();
    let rebuilt = build_reference_manifest(
        catalog,
        overlays,
        &rebuild_manifests,
        // Doctor is read-only and holds no view or build of its own, so it
        // reports the DURABLE reference set. A process-local root would make
        // the report depend on who asked.
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
    );
    match evaluate_history_gc(&generation_store, &rebuilt) {
        HistoryGcEnablementV1::Enabled { roots, divergence } => {
            let mut findings = vec![Finding::ok(format!(
                "repo-history GC enabled; {} generation(s) referenced",
                roots.len()
            ))];
            // D-038: a replaced stale index is INFO, never a failure. The
            // ordinary cause is an overlay swap or a `Ready` advancement,
            // neither of which writes the acceleration index; rendering that
            // as an action item would ask an operator to explain normal
            // operation.
            if let Some(divergence) = divergence {
                findings.push(Finding::info(format!(
                    "repo-history reference index refreshed: {}",
                    divergence.note
                )));
            }
            findings
        }
        HistoryGcEnablementV1::Disabled { diagnostic } => vec![Finding::action(
            format!("repo-history GC is disabled: {diagnostic}"),
            "inspect the generations root for a corrupt or unreachable reference-manifest.json; removing it lets the next evaluation rebuild from the catalog and overlay selectors",
        )],
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
            format!(
                "{} to build it (first build can take a while)",
                ops_call("bbox_reindex", "{}")
            ),
        )
    } else {
        Finding::ok(format!("{num_docs} indexed documents"))
    };
    SectionReport {
        section: "index",
        findings: vec![finding],
    }
}

fn vectors_section(_state: &crate::server::state::SharedState) -> SectionReport {
    let response = crate::embed_runtime::status_response_for_doctor();
    let mut findings = if response.routes.is_empty() {
        vec![Finding::info("no embedding routes active yet")]
    } else {
        response
            .routes
            .iter()
            .map(|(route, status)| classify_embed_route(route, status))
            .collect()
    };
    match bbox_vectors::metrics_nonblocking() {
        // No installed vector store means there is no loaded partition
        // inventory to diagnose yet. This is the same cold/empty state as the
        // embedding status above, not a route whose health was fabricated.
        None => {}
        Some(metrics) => {
            let routes = metrics.into_keys().take(64).collect::<Vec<_>>();
            match bbox_vectors::try_diagnostics_bounded(
                &routes,
                std::time::Duration::from_millis(250),
            ) {
                None => findings.push(Finding::warn(
                    "vector connectivity diagnostics unavailable: store is warming up",
                )),
                Some(Err(err)) => findings.push(Finding::warn(format!(
                    "vector connectivity diagnostics unavailable: {err:#}"
                ))),
                Some(Ok(report)) => {
                    let observations =
                        bbox_vectors::try_connectivity_observations().unwrap_or_default();
                    findings.extend(vector_connectivity_findings(
                        report,
                        &observations,
                        unix_now_secs(),
                    ));
                }
            }
        }
    }
    SectionReport {
        section: "vectors",
        findings,
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// How long a daily-maintenance connectivity measurement stands in for a
/// bounded doctor diagnostic that did not complete: one maintenance interval
/// plus slack.
const VECTOR_CONNECTIVITY_OBSERVATION_MAX_AGE_SECS: u64 = 48 * 3_600;

/// Classify one bounded connectivity diagnostic. A measured breach is an
/// action. A diagnostic that did not complete (deadline or lock contention)
/// is not evidence of a fault: it reports the daily maintenance measurement
/// when a recent one exists, and info otherwise.
fn vector_connectivity_findings(
    report: bbox_vectors::VectorDiagnosticsReport,
    observations: &std::collections::BTreeMap<String, bbox_vectors::ConnectivityObservation>,
    now_unix_secs: u64,
) -> Vec<Finding> {
    use bbox_vectors::VectorDiagnosticUnavailableReason as Reason;

    let repair_next = || {
        format!(
            "daily connectivity maintenance will attempt repair; inspect {} for current diagnostics",
            ops_call("bbox_embed_status", "{}")
        )
    };
    let mut findings = Vec::new();
    let mut degraded = false;
    for unavailable in report.unavailable {
        let route = unavailable.route;
        let reason = unavailable.reason.as_str();
        if !matches!(unavailable.reason, Reason::DeadlineExceeded | Reason::Busy) {
            findings.push(Finding::warn(format!(
                "vector connectivity unknown for {route}: {reason}"
            )));
            continue;
        }
        let recent = observations.get(&route).filter(|observation| {
            now_unix_secs.saturating_sub(observation.observed_unix_secs)
                <= VECTOR_CONNECTIVITY_OBSERVATION_MAX_AGE_SECS
        });
        findings.push(match recent {
            Some(observation) => {
                let age_hours =
                    now_unix_secs.saturating_sub(observation.observed_unix_secs) / 3_600;
                let measured = format!(
                    "{:.2}% zero-in-degree when daily maintenance measured it {age_hours}h ago",
                    observation.zero_in_degree_ratio * 100.0
                );
                if observation.breach && !observation.repaired {
                    degraded = true;
                    Finding::action(
                        format!(
                            "vector connectivity degraded for {route}: {measured}; this pass's bounded diagnostic did not complete ({reason})"
                        ),
                        repair_next(),
                    )
                } else if observation.repaired {
                    Finding::info(format!(
                        "vector connectivity for {route} was rebuilt by daily maintenance {age_hours}h ago; this pass's bounded diagnostic did not complete ({reason})"
                    ))
                } else {
                    Finding::info(format!(
                        "vector connectivity for {route}: {measured}; this pass's bounded diagnostic did not complete ({reason})"
                    ))
                }
            }
            None => Finding::info(format!(
                "vector connectivity unknown for {route}: bounded diagnostic did not complete ({reason}); not evidence of a fault"
            )),
        });
    }
    let mut checked = 0usize;
    for metrics in report.partitions.into_values() {
        let Some(hnsw) = metrics.hnsw else {
            continue;
        };
        checked += 1;
        if hnsw.connectivity_breach(bbox_vectors::NOTIFY_CONNECTIVITY_RATIO) {
            degraded = true;
            findings.push(Finding::action(
                format!(
                    "vector connectivity degraded for {}: {:.2}% zero-in-degree",
                    metrics.route,
                    hnsw.connectivity_risk_ratio() * 100.0
                ),
                repair_next(),
            ));
        }
    }
    if checked > 0
        && !degraded
        && findings
            .iter()
            .all(|finding| finding.level <= FindingLevel::Info)
    {
        findings.push(Finding::ok(format!(
            "HNSW connectivity diagnostics healthy across {checked} partition(s)"
        )));
    }
    findings
}

/// Snapshot and Git overlay health. The edge sidecar manifest is the
/// code-source activation authority and its Git overlay selectors are Git
/// source GC roots, so a manifest that fails to load or selects a missing
/// member is reported here. Read-only: one manifest read under the manifest
/// coordinator and the pinned read view's overlay map.
fn snapshots_section(state: &crate::server::state::SharedState) -> SectionReport {
    use bbox_edge_sidecar::manifest::{ManifestFallbackReason, try_load_manifest_index};

    let edges_dir = crate::server::edge_sidecar_dir(state);
    let mut findings = Vec::new();
    let manifest = bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        Ok(match try_load_manifest_index(&edges_dir) {
            Ok(index) => {
                let validation = index
                    .active_paths_for_loader(&edges_dir)
                    .map(|paths| paths.len());
                Some(Ok((index, validation)))
            }
            Err(ManifestFallbackReason::MissingNotMigrated) => None,
            Err(reason) => Some(Err(reason)),
        })
    });
    match manifest {
        Err(error) => findings.push(Finding::warn(format!(
            "snapshot manifest could not be read: {error:#}"
        ))),
        Ok(None) => findings.push(Finding::info(
            "no snapshot manifest yet (written by the first code-source activation or reindex)",
        )),
        Ok(Some(Err(reason))) => findings.push(Finding::warn(format!(
            "snapshot manifest is unavailable ({reason:?}); code-source activation and Git overlay selection cannot use it"
        ))),
        Ok(Some(Ok((index, validation)))) => {
            let workspaces = index.workspaces.len();
            let active = index
                .workspaces
                .values()
                .filter(|entry| entry.active_snapshot.is_some())
                .count();
            let dirty = index
                .workspaces
                .values()
                .filter(|entry| entry.dirty_overlay.is_some())
                .count();
            match validation {
                Ok(members) => findings.push(Finding::ok(format!(
                    "{workspaces} workspaces: {active} active snapshots, {dirty} dirty overlays, {members} selected members present"
                ))),
                Err(error) => findings.push(Finding::warn(format!(
                    "snapshot manifest selects a missing or invalid member: {error:#}"
                ))),
            }
        }
    }
    let overlays = state.code_read_view.read().git_overlays.len();
    findings.push(if overlays == 0 {
        Finding::info("no Git overlays pinned in the read view")
    } else {
        Finding::ok(format!(
            "{overlays} Git overlays pinned in the read view (Git source GC roots)"
        ))
    });
    SectionReport {
        section: "snapshots",
        findings,
    }
}

fn projects_section(state: &crate::server::state::SharedState) -> SectionReport {
    let records = state.records_provider.records_snapshot().records;
    let mut findings = Vec::new();
    let mut present = 0usize;
    // Catalog authority with no local attachments means this daemon holds no
    // checkouts at all (the cage topology): a path-existence check is
    // meaningless here, not a per-project warning.
    let paths_meaningful = state.project_authority.is_bridge();
    for record in records.iter() {
        if !paths_meaningful {
            present += 1;
            continue;
        }
        if std::path::Path::new(&record.canonical_path).exists() {
            present += 1;
        } else {
            findings.push(
                Finding::warn(format!(
                    "project `{}` path missing on disk: {}",
                    record.project_id, record.canonical_path
                ))
                .with_next(format!(
                    "{} if it moved, or {}",
                    ops_call(
                        "bbox_project_rename",
                        &format!(
                            "{{\"project\":\"{}\",\"new_path\":\"<new path>\"}}",
                            record.project_id
                        ),
                    ),
                    ops_call(
                        "bbox_project_unregister",
                        &format!("{{\"project\":\"{}\"}}", record.project_id),
                    ),
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
    // A records provider that served a stale projection or omitted rows
    // reports it here, so a silently thinned catalog projection is visible.
    if let Some(degradation) = state.records_provider.last_degradation() {
        findings.push(Finding::info(format!(
            "records provider degradation: {degradation}"
        )));
    }
    // The paired read that repository carriers depend on. A persistent
    // epoch disagreement leaves carriers at their last-good set rather than
    // encoding a moving Selected target, so it is a real degradation an
    // operator must see rather than a transient the runtime absorbs.
    if let Err(error) = crate::server::repo_io::CatalogBaseTargets::read_consistent_for_state(state)
    {
        findings.push(Finding::warn(format!(
            "catalog carrier paired read unavailable: {error:#}"
        )));
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

fn attention_section(state: &crate::server::state::SharedState) -> SectionReport {
    let mut findings = Vec::new();

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
                 embed.toml, then {}",
                ops_call("bbox_reembed", &format!("{{\"route\":\"{route}\"}}"))
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
                     embed.toml, then {}",
                    ops_call("bbox_reembed", &format!("{{\"route\":\"{route}\"}}"))
                ));
            }
        }
        return match reason {
            "credential_missing" => Finding::action(
                format!("route `{route}` is missing provider credentials: {detail}"),
                format!(
                    "set the provider API key env, then {}",
                    ops_call("bbox_reembed", &format!("{{\"route\":\"{route}\"}}"))
                ),
            ),
            "queue_full" => Finding::warn(format!(
                "route `{route}` queue is full ({} pending, {} bytes)",
                status.queue_depth, status.queue_bytes
            )),
            _ => Finding::action(
                format!("route `{route}` is unavailable: {detail}"),
                format!(
                    "fix the route config/provider, then {}",
                    ops_call("bbox_reembed", &format!("{{\"route\":\"{route}\"}}"))
                ),
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

// Test-only, and deliberately placed immediately above the file's existing
// test module rather than beside the sections it renders. The catalog
// ownership ratchet truncates each file at its FIRST `#[cfg(test)]`, so a
// test-only item inserted mid-file silently drops every tracked pattern
// below it from the count and reads as a baseline shrink. Keeping the
// truncation point where it was keeps the Phase 6 deletion inventory honest.
/// The catalog sections' rendered findings, for tests that need to assert
/// what an operator would actually see rather than what the projection
/// carries. Kept beside the sections so it cannot drift from them.
#[cfg(test)]
pub(crate) fn catalog_sections_for_test(state: &crate::server::state::SharedState) -> Vec<String> {
    let Some(statuses) = catalog_project_statuses(state) else {
        return Vec::new();
    };
    [
        accepted_publication_section(&statuses),
        publisher_binding_section(&statuses),
        overlay_baseline_section(&statuses),
        attachment_capability_section(&statuses),
        artifact_watcher_section(&statuses),
    ]
    .iter()
    .flat_map(|section| {
        section
            .findings
            .iter()
            .map(|finding| finding.message.clone())
    })
    .collect()
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

    /// The snapshots section reads the manifest and the pinned overlay map,
    /// never an edge graph: an absent manifest is informational, a valid
    /// one is counted, and a manifest selecting a missing snapshot warns.
    #[test]
    fn snapshots_section_reports_manifest_and_overlay_health() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let state = crate::server::state::SharedState::for_test(&root);
        let levels = |report: &SectionReport| {
            report
                .findings
                .iter()
                .map(|finding| (finding.level, finding.message.clone()))
                .collect::<Vec<_>>()
        };

        let absent = snapshots_section(&state);
        assert_eq!(absent.section, "snapshots");
        assert_eq!(absent.findings[0].level, FindingLevel::Info, "{absent:?}");
        assert!(absent.findings[0].message.contains("no snapshot manifest"));

        let edges_dir = crate::server::edge_sidecar_dir(&state);
        std::fs::create_dir_all(&edges_dir).unwrap();
        let edge = bbox_edge_sidecar::edge_sidecar::Edge {
            source: crate::entity_ref::EntityRef::Knowledge { id: "a".into() },
            kind: "DESCRIBES".into(),
            target: crate::entity_ref::EntityRef::Knowledge { id: "b".into() },
            provenance: bbox_chunker::EdgeProvenance::Derived,
            confidence: bbox_chunker::EdgeConfidence::Exact,
            metadata: Default::default(),
            project_id: None,
        };
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            &edges_dir,
            "p",
            "repo",
            Some("main"),
            "head-a",
            vec![edge],
            vec![],
            vec![],
        )
        .unwrap();
        let healthy = snapshots_section(&state);
        assert_eq!(
            healthy.findings[0].level,
            FindingLevel::Ok,
            "{:?}",
            levels(&healthy)
        );
        assert!(
            healthy.findings[0]
                .message
                .starts_with("1 workspaces: 1 active snapshots, 0 dirty overlays"),
            "{:?}",
            levels(&healthy)
        );
        assert!(
            healthy.findings[1]
                .message
                .contains("no Git overlays pinned")
        );

        let index = bbox_edge_sidecar::manifest::ManifestIndex::load(&edges_dir).unwrap();
        let active = index.workspaces["p"].active_snapshot.clone().unwrap();
        std::fs::remove_dir_all(
            bbox_edge_sidecar::manifest::materialized_dir(&edges_dir).join(&active),
        )
        .unwrap();
        let broken = snapshots_section(&state);
        assert_eq!(
            broken.findings[0].level,
            FindingLevel::Warn,
            "{:?}",
            levels(&broken)
        );
        assert!(
            broken.findings[0].message.contains("snapshot manifest"),
            "{:?}",
            levels(&broken)
        );
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
                "code_sources",
                "vectors",
                "snapshots",
                "projects",
                "checkout_access",
                "memories",
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

    /// A14: a requested section validates against the applicable vocabulary
    /// (bridge modes have no catalog sections) and then collects exactly one
    /// section instead of the full report.
    #[test]
    fn run_section_collects_only_the_requested_section() {
        crate::init_system_memory_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let server = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(tmp.path()),
        ));
        let names = section_names(&server.state);
        assert_eq!(names.first(), Some(&"daemon"));
        assert!(names.contains(&"attention"));
        assert!(
            !names.contains(&"knowledge_transport"),
            "bridge test state has no catalog sections"
        );
        let report = run_section(&server, "attention").expect("focused doctor section");
        assert_eq!(report.sections.len(), 1);
        assert_eq!(report.sections[0].section, "attention");
        assert!(!report.sections[0].findings.is_empty());
        let bridge_only = run_section(&server, "knowledge_transport").unwrap_err();
        assert!(
            bridge_only.to_string().contains("Unknown section"),
            "catalog section must be refused in bridge mode: {bridge_only}"
        );
        let retired = run_section(&server, "resolver_compat").unwrap_err();
        assert!(
            retired.to_string().contains("Unknown section"),
            "the resolver compatibility section is retired: {retired}"
        );
        let unknown = run_section(&server, "not-a-section").unwrap_err();
        assert!(
            unknown
                .to_string()
                .contains("Unknown section `not-a-section`")
        );
    }

    #[test]
    fn checkout_access_json_is_complete_bounded_and_path_free() {
        use bbox_indexing::checkout_access::{
            CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
            CheckoutAccessSourceLane, CheckoutAttachmentSelector,
        };

        crate::init_system_memory_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project_root = root.join("project");
        std::fs::create_dir(&project_root).unwrap();
        let state = std::sync::Arc::new(crate::server::state::SharedState::for_test(&root));
        let project = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&project_root)
            .unwrap();
        let request = |project_id: String, kind| CheckoutAccessRequest {
            project_id,
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        };
        state
            .checkout_access
            .acquire(request(
                project.project_id.clone(),
                CheckoutAccessKind::LocalProjectWalk,
            ))
            .unwrap();
        state
            .checkout_access
            .acquire(request("missing-project".into(), CheckoutAccessKind::Blame))
            .unwrap_err();

        let server = crate::server::BlackboxServer::new(state);
        let report = run(&server).unwrap();
        let health = report.checkout_access.as_ref().unwrap();
        assert_eq!(health.sequence, 2);
        assert_eq!(
            health
                .operations
                .iter()
                .map(|operation| operation.kind)
                .collect::<Vec<_>>(),
            CheckoutAccessKind::ALL.to_vec()
        );
        let local = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::LocalProjectWalk)
            .unwrap();
        assert_eq!(local.granted, 1);
        assert_eq!(local.denied, 0);
        assert!(local.last_success_unix_secs.is_some());
        let blame = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::Blame)
            .unwrap();
        assert_eq!(blame.granted, 0);
        assert_eq!(blame.denied, 1);
        assert_eq!(blame.last_success_unix_secs, None);
        assert_eq!(
            health.active_compatibility_lanes,
            vec![CheckoutAccessSourceLane::LegacyProjectRecord]
        );
        assert!(
            health.counters.len()
                <= CheckoutAccessKind::ALL.len() * CheckoutAccessSourceLane::ALL.len() * 2
        );

        let projection = serde_json::to_value(health).unwrap();
        let allowed_counter_fields = std::collections::BTreeSet::from([
            "kind",
            "source_lane",
            "outcome",
            "count",
            "last_sequence",
            "last_unix_secs",
        ]);
        for counter in projection["counters"].as_array().unwrap() {
            assert_eq!(
                counter
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                allowed_counter_fields
            );
        }
        let serialized = serde_json::to_string(&projection).unwrap();
        assert!(!serialized.contains(root.to_string_lossy().as_ref()));
        assert!(!serialized.contains(&project.project_id));
        assert!(!serialized.contains("missing-project"));
        assert_eq!(
            serde_json::to_value(&report).unwrap()["checkout_access"],
            projection
        );
        assert!(serde_json::to_value(&report).unwrap()["knowledge_transport"].is_null());

        let checkout_section = report
            .sections
            .iter()
            .find(|section| section.section == "checkout_access")
            .unwrap();
        assert!(
            checkout_section
                .findings
                .iter()
                .any(|finding| finding.message.contains("1 granted, 0 denied"))
        );
        assert!(
            checkout_section
                .findings
                .iter()
                .any(|finding| finding.message.contains("0 granted, 1 denied"))
        );
        assert!(checkout_section.findings.iter().any(|finding| {
            finding
                .message
                .contains("active checkout compatibility lanes: legacy_project_record")
        }));
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

    fn unavailable(
        route: &str,
        reason: bbox_vectors::VectorDiagnosticUnavailableReason,
    ) -> bbox_vectors::VectorDiagnosticUnavailable {
        bbox_vectors::VectorDiagnosticUnavailable {
            route: route.into(),
            reason,
        }
    }

    fn healthy_partition(route: &str) -> bbox_vectors::PartitionMetrics {
        bbox_vectors::PartitionMetrics {
            route: route.into(),
            state: bbox_vectors::PartitionState::Active { dims: 2 },
            dims: 2,
            wal_records: 0,
            active_count: 1_000,
            deleted_count: 0,
            deleted_ratio: 0.0,
            hnsw_rebuilds: 0,
            hnsw: Some(bbox_vectors::HnswMetricsSerde {
                total_nodes: 1_000,
                active_nodes: 1_000,
                deleted_nodes: 0,
                dimensions: 2,
                max_level: 1,
                entry_point: Some(0),
                neighbor_refs: 16_000,
                avg_neighbor_degree: 16.0,
                layer_distribution: vec![1_000],
                disconnected_nodes: 0,
                zero_in_degree_nodes: 0,
            }),
        }
    }

    /// A bounded diagnostic that ran out of time measured nothing: info, and
    /// the measured healthy partition still reports ok.
    #[test]
    fn vector_diagnostic_deadline_is_info_not_warn() {
        use bbox_vectors::VectorDiagnosticUnavailableReason as Reason;

        let mut report = bbox_vectors::VectorDiagnosticsReport::default();
        report
            .partitions
            .insert("small".into(), healthy_partition("small"));
        report.unavailable = vec![
            unavailable("large", Reason::DeadlineExceeded),
            unavailable("busy", Reason::Busy),
        ];
        let findings =
            vector_connectivity_findings(report, &std::collections::BTreeMap::new(), 1_000_000);
        let levels = findings
            .iter()
            .map(|finding| (finding.level, finding.message.as_str()))
            .collect::<Vec<_>>();
        assert!(
            findings
                .iter()
                .filter(
                    |finding| finding.message.contains("large") || finding.message.contains("busy")
                )
                .all(|finding| finding.level == FindingLevel::Info),
            "{levels:?}"
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.level == FindingLevel::Ok
                    && finding.message.contains("across 1 partition")),
            "{levels:?}"
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.level <= FindingLevel::Info),
            "{levels:?}"
        );
    }

    /// A missing graph is observed state, not an incomplete diagnostic.
    #[test]
    fn vector_missing_graph_still_warns() {
        let report = bbox_vectors::VectorDiagnosticsReport {
            unavailable: vec![unavailable(
                "graphless",
                bbox_vectors::VectorDiagnosticUnavailableReason::MissingGraph,
            )],
            ..Default::default()
        };
        let findings =
            vector_connectivity_findings(report, &std::collections::BTreeMap::new(), 1_000_000);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].level, FindingLevel::Warn);
    }

    /// A recent daily-maintenance measurement stands in for an incomplete
    /// diagnostic: a measured breach is an action, a healthy or repaired
    /// measurement is info, and a stale one is ignored.
    #[test]
    fn vector_diagnostic_deadline_prefers_recent_maintenance_measurement() {
        use bbox_vectors::{ConnectivityObservation, VectorDiagnosticUnavailableReason as Reason};

        let now = 10_000_000;
        let observation = |hours_ago: u64, breach: bool, repaired: bool| ConnectivityObservation {
            observed_unix_secs: now - hours_ago * 3_600,
            zero_in_degree_ratio: if breach { 0.08 } else { 0.001 },
            breach,
            repaired,
        };
        let observations = [
            ("breached".to_string(), observation(3, true, false)),
            ("healthy".to_string(), observation(3, false, false)),
            ("repaired".to_string(), observation(3, true, true)),
            ("stale".to_string(), observation(72, true, false)),
        ]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
        let report = bbox_vectors::VectorDiagnosticsReport {
            unavailable: ["breached", "healthy", "repaired", "stale"]
                .into_iter()
                .map(|route| unavailable(route, Reason::DeadlineExceeded))
                .collect(),
            ..Default::default()
        };
        let findings = vector_connectivity_findings(report, &observations, now);
        let level_of = |route: &str| {
            findings
                .iter()
                .find(|finding| finding.message.contains(&format!(" {route}")))
                .map(|finding| (finding.level, finding.message.clone()))
                .unwrap_or_else(|| panic!("{route} reported: {findings:?}"))
        };
        let (level, message) = level_of("breached");
        assert_eq!(level, FindingLevel::Action, "{message}");
        assert!(message.contains("8.00% zero-in-degree"), "{message}");
        assert!(message.contains("3h ago"), "{message}");
        assert_eq!(level_of("healthy").0, FindingLevel::Info);
        let (level, message) = level_of("repaired");
        assert_eq!(level, FindingLevel::Info, "{message}");
        assert!(message.contains("rebuilt"), "{message}");
        let (level, message) = level_of("stale");
        assert_eq!(level, FindingLevel::Info, "{message}");
        assert!(message.contains("unknown"), "{message}");
    }

    fn degraded_counter(
        project_id: &str,
        count: u64,
        last_unix_secs: u64,
    ) -> bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationCounterV1 {
        use bbox_indexing::knowledge_transport_observations::{
            KnowledgeTransportOperationCounterV1, KnowledgeTransportOperationV1,
            KnowledgeTransportOutcomeV1,
        };
        KnowledgeTransportOperationCounterV1 {
            project_id: project_id.into(),
            operation: KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
            outcome: KnowledgeTransportOutcomeV1::Degraded,
            count,
            first_sequence: 1,
            last_sequence: count,
            last_unix_secs,
        }
    }

    /// Degraded counters are lifetime totals: only one that advanced within
    /// the last day warns; old ones report the lifetime total as info.
    #[test]
    fn degraded_knowledge_transport_warns_only_on_recent_counters() {
        let now = 1_790_782_254;
        let old = [
            degraded_counter("p_old_a", 800, now - 50 * 86_400),
            degraded_counter("p_old_b", 400, now - 10 * 86_400),
        ];
        let finding = degraded_transport_finding(old.iter(), now).expect("finding");
        assert_eq!(finding.level, FindingLevel::Info, "{}", finding.message);
        assert!(
            finding.message.contains("1200 lifetime"),
            "{}",
            finding.message
        );
        assert!(finding.message.contains("p_old_b"), "{}", finding.message);

        let mut mixed = old.to_vec();
        mixed.push(degraded_counter("p_recent", 39, now - 2 * 3_600));
        let finding = degraded_transport_finding(mixed.iter(), now).expect("finding");
        assert_eq!(finding.level, FindingLevel::Warn, "{}", finding.message);
        assert!(finding.message.contains("p_recent"), "{}", finding.message);
        assert!(
            finding.message.contains("1239 lifetime"),
            "{}",
            finding.message
        );

        assert!(degraded_transport_finding(std::iter::empty(), now).is_none());
    }

    /// No code-source lane by design is info; a project that has a lane and
    /// no source is still a warning with the remedy.
    #[test]
    fn source_unavailable_warns_only_for_projects_with_a_code_source_lane() {
        let diagnostic = "project has no attached checkout and no active collected generation";
        let without_lane = source_unavailable_finding("p_remote", diagnostic, false);
        assert_eq!(without_lane.level, FindingLevel::Info);
        assert!(without_lane.next.is_none());
        assert!(without_lane.message.contains("no code-source lane"));
        let with_lane = source_unavailable_finding("p_collected", diagnostic, true);
        assert_eq!(with_lane.level, FindingLevel::Warn);
        assert!(with_lane.next.is_some());
    }
}

#[cfg(test)]
mod catalog_health_tests {
    use super::*;
    use crate::server::state::ProjectRuntimeStatus;
    use crate::server::state::catalog_fixture::{
        COMMIT_ONE, COMMIT_TWO, CatalogFixture, knowledge_entry,
    };

    const PROJECT: &str = "p_health";

    fn status(server: &crate::server::BlackboxServer) -> ProjectRuntimeStatus {
        server
            .state
            .project_runtime_status(PROJECT)
            .expect("catalog mode projects a status")
    }

    fn section<'a>(report: &'a DoctorReport, name: &str) -> &'a SectionReport {
        report
            .sections
            .iter()
            .find(|section| section.section == name)
            .unwrap_or_else(|| panic!("section {name} is present"))
    }

    fn messages(report: &DoctorReport, name: &str) -> String {
        section(report, name)
            .findings
            .iter()
            .map(|finding| finding.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn catalog_findings_beyond_twenty_remain_exactly_recoverable() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let base = status(&fixture.server());
        let mut statuses = (0..35)
            .map(|index| {
                let mut row = base.clone();
                row.project_id = format!("p_health_{index:02}");
                row.accepted.state = "missing";
                row
            })
            .collect::<Vec<_>>();
        statuses[34].accepted.state = "corrupt";
        statuses[34].accepted.diagnostic = Some("late-诊断-\"\n".repeat(2000));
        let section = accepted_publication_section(&statuses);
        assert_eq!(section.findings.len(), 35);
        assert_eq!(section.worst(), FindingLevel::Blocked);
        let report = serde_json::to_value(DoctorReport::from_sections(vec![section])).unwrap();
        let mut recovered = String::new();
        let mut cursor = None;
        loop {
            let page = bbox_corpus_core::response_page::json_body_page(
                "doctor",
                &report,
                cursor.as_deref(),
                Some(257),
            )
            .unwrap();
            recovered.push_str(page["text"].as_str().unwrap());
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered).unwrap(),
            report
        );
        assert!(recovered.contains("p_health_34"));
    }

    /// Accepted Current, and the section says so without inventing findings.
    #[test]
    fn accepted_current_is_healthy_and_advance_is_available() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("k1", "a")],
            &[],
        );
        let server = fixture.server();

        let status = status(&server);
        assert_eq!(status.accepted.state, "current");
        assert!(status.accepted.serves_published_content);
        assert!(status.accepted.advance_available);
        assert_eq!(status.accepted.scope_agreement, "agreed");
        assert!(status.accepted.generation_id.is_some());
        assert!(status.accepted.last_verified_unix_secs.is_some());

        let report = run(&server).unwrap();
        assert!(report.knowledge_transport.is_some());
        assert_eq!(
            section(&report, "knowledge_transport").worst(),
            FindingLevel::Info
        );
        assert!(
            section(&report, "accepted_publication").worst() == FindingLevel::Ok,
            "{}",
            messages(&report, "accepted_publication")
        );
    }

    /// Accepted Prior: reads continue, mutation refuses, and the finding
    /// says both.
    #[test]
    fn accepted_prior_reports_repair_required() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("k1", "a")],
            &[],
        );
        let second = fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_TWO,
            &[knowledge_entry("k1", "b")],
            &[],
        );
        fixture.corrupt_generation(PROJECT, &second.generation_id);
        let server = fixture.server();

        let status = status(&server);
        assert_eq!(status.accepted.state, "prior");
        assert!(status.accepted.serves_published_content);
        assert!(!status.accepted.advance_available);

        let report = run(&server).unwrap();
        let text = messages(&report, "accepted_publication");
        assert!(text.contains("PRIOR"), "{text}");
    }

    /// A project with no pointer at all is Missing, not Corrupt.
    #[test]
    fn accepted_missing_is_distinct_from_corrupt() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let status = status(&server);
        assert_eq!(status.accepted.state, "missing");
        assert!(!status.accepted.serves_published_content);
        assert_eq!(status.binding.status, "unbound");
    }

    /// Both arms damaged is Corrupt and blocks.
    #[test]
    fn accepted_corrupt_blocks() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let first = fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("k1", "a")],
            &[],
        );
        let second = fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_TWO,
            &[knowledge_entry("k1", "b")],
            &[],
        );
        fixture.corrupt_generation(PROJECT, &first.generation_id);
        fixture.corrupt_generation(PROJECT, &second.generation_id);
        let server = fixture.server();

        let status = status(&server);
        assert_eq!(status.accepted.state, "corrupt");
        assert!(!status.accepted.serves_published_content);

        let report = run(&server).unwrap();
        assert_eq!(
            section(&report, "accepted_publication").worst(),
            FindingLevel::Blocked,
            "{}",
            messages(&report, "accepted_publication")
        );
    }

    /// Scope migration leaves accepted content readable at its old scope and
    /// reports the bridge as an action (plan 4.9).
    #[test]
    fn scope_migration_reports_refresh_required() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("k1", "a")],
            &[],
        );
        fixture.migrate_project_scope(PROJECT, &CatalogFixture::scope("nested"));
        let server = fixture.server();

        let status = status(&server);
        assert_eq!(status.accepted.scope_agreement, "refresh_required");
        assert_eq!(
            status
                .accepted
                .accepted_scope
                .as_ref()
                .unwrap()
                .bbox_root_relpath,
            ".",
            "response provenance keeps the OLD accepted scope"
        );
        assert_eq!(
            status.catalog_scope.as_ref().unwrap().bbox_root_relpath,
            "nested"
        );

        let report = run(&server).unwrap();
        let text = messages(&report, "accepted_publication");
        assert!(text.contains("migrated"), "{text}");
    }

    /// Binding Attached vs Detached. Detached is D-033 item 1 made
    /// observable: the pointer outlives its attachment and an explicit bind
    /// repairs it.
    #[test]
    fn binding_reports_attached_then_detached_after_detach() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        fixture.attach_overlay_checkout(
            PROJECT,
            &scope,
            &checkout,
            CatalogFixture::attachment().as_str(),
            "cccccccccccccccccccccccccccccc01",
            true,
        );
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("k1", "a")],
            &[],
        );
        let server = fixture.server();
        assert_eq!(status(&server).binding.status, "attached");

        CatalogFixture::detach_in_server(&server, CatalogFixture::attachment().as_str());
        let detached = status(&server);
        assert_eq!(detached.binding.status, "detached");
        assert!(
            detached.accepted.serves_published_content,
            "detach preserves accepted content"
        );

        let report = run(&server).unwrap();
        let text = messages(&report, "publisher_binding");
        assert!(text.contains("DETACHED"), "{text}");
    }

    /// Capability availability comes from the catalog bits, and a
    /// remote-only project reports no attachment rather than a denial.
    #[test]
    fn capability_availability_reads_catalog_bits_without_synthesizing_denials() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let server = fixture.server();
        assert!(status(&server).attachments.is_empty());

        let report = run(&server).unwrap();
        let text = messages(&report, "attachment_capability");
        assert!(text.contains("remote-only"), "{text}");
        assert!(
            !text.contains("denied"),
            "no operation was attempted, so nothing is a denial: {text}"
        );
    }

    /// No watcher in this process is an informational state about the
    /// process, never an "unregistered" verdict about an attachment.
    #[test]
    fn watcher_absent_is_informational_not_a_project_fault() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let server = fixture.server();

        let status = status(&server);
        assert!(!status.watcher.watcher_running);
        assert!(status.watcher.capable_but_unregistered.is_empty());

        let report = run(&server).unwrap();
        assert_eq!(
            section(&report, "artifact_watcher").worst(),
            FindingLevel::Info
        );
    }

    /// Plan 13.6: no absolute path appears anywhere in the serialized
    /// report. The fixture deliberately holds a real checkout so a leak
    /// would have something to leak.
    #[test]
    fn catalog_health_serialization_is_path_free() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().canonicalize().unwrap().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        fixture.attach_overlay_checkout(
            PROJECT,
            &scope,
            &checkout,
            CatalogFixture::attachment().as_str(),
            "cccccccccccccccccccccccccccccc01",
            true,
        );
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("k1", "a")],
            &[],
        );
        let server = fixture.server();

        let status = serde_json::to_string(&status(&server)).unwrap();
        let needle = checkout.to_string_lossy().into_owned();
        assert!(
            !status.contains(&needle),
            "project runtime status leaked a checkout path: {status}"
        );

        let report = run(&server).unwrap();
        let rendered = serde_json::to_string(&report).unwrap();
        assert!(
            !rendered.contains(&needle),
            "doctor report leaked a checkout path"
        );
        assert!(
            !report.render_summary().contains(&needle),
            "doctor summary leaked a checkout path"
        );
    }

    /// Bridge mode omits the catalog sections entirely rather than
    /// rendering them empty.
    #[test]
    fn bridge_mode_omits_the_catalog_health_sections() {
        crate::init_system_memory_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let state = std::sync::Arc::new(crate::server::state::SharedState::for_test(&root));
        let server = crate::server::BlackboxServer::new(state);

        let report = run(&server).unwrap();
        for name in [
            "accepted_publication",
            "publisher_binding",
            "overlay_baseline",
            "attachment_capability",
            "artifact_watcher",
        ] {
            assert!(
                report
                    .sections
                    .iter()
                    .all(|section| section.section != name),
                "bridge mode must not render {name}"
            );
        }
    }

    const OTHER_PROJECT: &str = "p_health_other";
    const PRODUCER: &str = "producer-health";

    /// A catalog server whose producer table assigns `assigned` scopes to
    /// `PRODUCER`.
    fn assigned_server(
        fixture: &CatalogFixture,
        assigned: &[(&str, bbox_corpus_core::identity::PublishedScope)],
    ) -> crate::server::BlackboxServer {
        use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};

        let state = crate::server::state::SharedState::for_test_catalog(
            fixture.root(),
            &fixture.root().join("catalog").join("projects.json"),
        );
        let token = bro_rpc::ServiceToken::parse("f".repeat(64)).unwrap();
        state
            .code_sources
            .install_auth_for_test(std::sync::Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: PRODUCER.into(),
                        projects: assigned
                            .iter()
                            .map(|(project_id, scope)| (scope.clone(), project_id.to_string()))
                            .collect(),
                    },
                )],
            )));
        crate::server::BlackboxServer::new(std::sync::Arc::new(state))
    }

    /// The locality cutovers are retired: neither a catalog daemon (with
    /// legacy marker files still in its state root) nor a bridge daemon
    /// reports a locality section, and asking for one is refused.
    #[test]
    fn doctor_has_no_locality_section() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        for name in [
            "render-locality-cutover-marker.json",
            "code-source-locality-cutover-marker.json",
        ] {
            std::fs::write(fixture.root().join(name), b"{ not json").unwrap();
        }
        let catalog = fixture.server();

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let bridge = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(&root),
        ));
        for server in [&catalog, &bridge] {
            assert!(
                !section_names(&server.state)
                    .iter()
                    .any(|name| name.contains("locality")),
                "{:?}",
                section_names(&server.state)
            );
            let report = run(server).unwrap();
            assert!(
                report
                    .sections
                    .iter()
                    .all(|section| !section.section.contains("locality"))
            );
            let refused = run_section(server, "locality_cutovers").unwrap_err();
            assert!(refused.to_string().contains("Unknown section"), "{refused}");
        }
    }

    /// Only a project with neither an attached checkout nor a code-source
    /// producer assignment lacks a code-source lane.
    #[test]
    fn code_source_lane_requires_an_attachment_or_a_producer_assignment() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        fixture.add_published_project(OTHER_PROJECT, &CatalogFixture::scope("other"));
        let server = assigned_server(&fixture, &[(OTHER_PROJECT, CatalogFixture::scope("other"))]);
        let without_lane = projects_without_code_source_lane(&server.state);
        assert!(without_lane.contains(PROJECT), "{without_lane:?}");
        assert!(!without_lane.contains(OTHER_PROJECT), "{without_lane:?}");
    }

    /// A project that can never accept a candidate (no published scope, no
    /// attachment) reports its missing pointer as having no lane, not as
    /// waiting for the owning producer.
    #[test]
    fn accepted_missing_without_a_publication_lane_is_reported_as_by_design() {
        crate::init_system_memory_for_tests();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let published = status(&fixture.server());
        assert_eq!(published.accepted.state, "missing");
        let mut connector = published.clone();
        connector.project_id = "p_connector".into();
        connector.catalog_scope = None;
        connector.attachments.clear();

        let section = accepted_publication_section(&[published, connector]);
        let text = section
            .findings
            .iter()
            .map(|finding| format!("{:?} {}", finding.level, finding.message))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            section
                .findings
                .iter()
                .all(|finding| finding.level == FindingLevel::Info),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "project {PROJECT} has no accepted publication pointer; its first valid"
            )),
            "{text}"
        );
        assert!(
            text.contains("project p_connector has no accepted publication lane"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod history_activation_deadletter_tests {
    use super::*;

    #[test]
    fn deadlettered_activation_surfaces_with_the_retirement_remedy() {
        crate::init_system_memory_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let state = std::sync::Arc::new(crate::server::state::SharedState::for_test(&root));
        assert!(
            history_activation_deadletter_findings(&state).is_empty(),
            "a never-provisioned dead-letter area reports nothing"
        );

        let repo_history = bbox_corpus_core::project_catalog::RepoHistoryId::parse(
            "rh_00000000000000000000000000000042",
        )
        .unwrap();
        let source = format!("ghs_{}", "a".repeat(64));
        state
            .git_sources
            .store()
            .record_activation_deadletter(
                &repo_history,
                "producer-a",
                &source,
                "repo_history_not_found",
                None,
            )
            .unwrap();

        let section = code_sources_section(&state);
        let finding = section
            .findings
            .iter()
            .find(|finding| finding.message.contains(repo_history.as_str()))
            .expect("the dead letter surfaces in the code_sources section");
        assert_eq!(finding.level, FindingLevel::Action);
        assert!(
            finding.message.contains("dead-lettered"),
            "{}",
            finding.message
        );
        assert!(finding.message.contains(&source), "{}", finding.message);
        let next = finding.next.as_deref().unwrap_or_default();
        assert!(
            next.contains(&format!(
                "blackbox git-history activations-drop --repo-history {}",
                repo_history.as_str()
            )) && next.contains("--retire-ready-pointer"),
            "{next}"
        );

        state
            .git_sources
            .store()
            .drop_activation_deadletter(&repo_history)
            .unwrap();
        assert!(history_activation_deadletter_findings(&state).is_empty());
    }
}
