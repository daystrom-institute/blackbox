//! `bbox_doctor` v0: one read-only "what do I need to know right now?"
//! surface (design/operations/config-artifacts/ops-artifact-bundles-and-doctor.md,
//! Phase 5 pulled forward). Aggregates existing health signals in-process
//! and classifies findings; it never mutates stores, enqueues notes, or
//! emits inbox items.
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
        }
    }

    fn with_checkout_access(
        mut self,
        health: bbox_indexing::checkout_access::CheckoutAccessHealth,
    ) -> Self {
        self.checkout_access = Some(health);
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
        graph_section(server),
        projects_section(state),
    ];
    let checkout_access = state.checkout_access_observations.health();
    sections.push(checkout_access_section(&checkout_access));
    sections.push(resolver_compat_section(state));
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
    sections.extend([
        memories_section(state),
        knowledge_section(state),
        attention_section(state),
    ]);
    Ok(DoctorReport::from_sections(sections).with_checkout_access(checkout_access))
}

/// How many per-project findings one catalog section emits before it
/// summarizes the rest. Doctor output is an operator surface, not a
/// dump; the projection itself stays complete for programmatic consumers.
const MAX_PROJECT_FINDINGS: usize = 20;

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

/// Append a bounded tail line when a section had more to say than it showed.
fn bound_findings(findings: &mut Vec<Finding>, considered: usize, section: &str) {
    if considered > MAX_PROJECT_FINDINGS {
        findings.push(Finding::info(format!(
            "{} more {section} findings not shown",
            considered - MAX_PROJECT_FINDINGS
        )));
    }
}

/// Accepted-publication state per project, plus the two states that read
/// fine but refuse mutation: Prior fallback and the scope-migration bridge.
fn accepted_publication_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut considered = 0;
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
            considered += 1;
            if considered <= MAX_PROJECT_FINDINGS {
                let project = &status.project_id;
                findings.push(Finding::action(
                    format!(
                        "project {project} could not be read from the catalog pair; \
                         its catalog-derived status is unavailable, not denied"
                    ),
                    "bbox_doctor",
                ));
                findings.push(match status.accepted.state {
                    "current" if status.accepted.serves_published_content => {
                        Finding::info(format!(
                            "project {project} accepted publication is verified independently \
                             of the catalog and is CURRENT; published knowledge and gaps keep \
                             serving while the catalog pair is unreadable"
                        ))
                    }
                    "prior" => Finding::action(
                        format!(
                            "project {project} accepted publication is verified independently \
                             of the catalog and fell back to its PRIOR generation; reads \
                             continue and every mutation refuses until repair"
                        ),
                        "bbox_project_publisher_status",
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
                        "bbox_project_publisher_status",
                    ),
                    other => Finding::warn(format!(
                        "project {project} accepted publication state is {other} and could \
                         not be evaluated further while the catalog pair is unreadable"
                    )),
                });
            }
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
        considered += 1;
        if considered > MAX_PROJECT_FINDINGS {
            continue;
        }
        let project = &status.project_id;
        findings.push(match status.accepted.state {
            // Serving old accepted truth under its old scope until a
            // new-scope advance clears the bridge (plan 4.9).
            _ if status.accepted.scope_agreement == "refresh_required" => Finding::action(
                format!(
                    "project {project} serves accepted content at a scope the catalog has since \
                     migrated; publishing at the current scope clears the bridge"
                ),
                "bbox_project_publisher_advance",
            ),
            // Reads continue off the prior arm; every mutation refuses.
            "prior" => Finding::action(
                format!(
                    "project {project} fell back to its PRIOR accepted generation; reads continue \
                     and establish, bind, and advance all refuse until repair"
                ),
                "bbox_project_publisher_status",
            ),
            "missing" => Finding::info(format!(
                "project {project} has no accepted publication pointer; an explicit establish \
                 creates the first one"
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
    bound_findings(&mut findings, considered, "accepted-publication");
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

/// Which attachment each pointer names and whether an advance can run.
fn publisher_binding_section(
    statuses: &[crate::server::state::ProjectRuntimeStatus],
) -> SectionReport {
    let mut findings = Vec::new();
    let mut considered = 0;
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
                "bbox_project_publisher_bind",
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
            considered += 1;
            if considered <= MAX_PROJECT_FINDINGS {
                findings.push(finding);
            }
        }
    }
    bound_findings(&mut findings, considered, "publisher-binding");
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
    let mut considered = 0;
    let mut fresh = 0;
    for status in statuses {
        for overlay in &status.overlays {
            if overlay.outcome == "fresh" {
                fresh += 1;
                continue;
            }
            considered += 1;
            if considered > MAX_PROJECT_FINDINGS {
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
    bound_findings(&mut findings, considered, "overlay-baseline");
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
    let mut considered = 0;
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
            considered += 1;
            if considered > MAX_PROJECT_FINDINGS {
                continue;
            }
            findings.push(Finding::warn(format!(
                "project {} attachment {} records no capabilities; every checkout-backed lane \
                 degrades for it",
                status.project_id, attachment.attachment_id
            )));
        }
    }
    bound_findings(&mut findings, considered, "attachment-capability");
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
    let mut considered = 0;
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
        if status.watcher.capable_but_unregistered.is_empty() {
            continue;
        }
        considered += 1;
        if considered > MAX_PROJECT_FINDINGS {
            continue;
        }
        findings.push(Finding::warn(format!(
            "project {} has {} attachment(s) recording artifact_watching with no live watcher \
             registration",
            status.project_id,
            status.watcher.capable_but_unregistered.len()
        )));
    }
    bound_findings(&mut findings, considered, "artifact-watcher");
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

/// Resolver compatibility-lane counters (phase-2 §9.2): the per-surface
/// observations the Phase 6 compatibility cut consumes. Also surfaces the
/// records-provider's most recent degradation (stale projection serving,
/// omitted rows) so a silently-thinned catalog projection is visible.
fn resolver_compat_section(state: &crate::server::state::SharedState) -> SectionReport {
    let snapshot = state.resolver_compat.snapshot();
    let mut findings = Vec::new();
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
    if snapshot.sequence == 0 {
        findings.push(Finding::info(
            "no resolver compatibility lane has fired yet",
        ));
    } else {
        for (surface, lanes) in &snapshot.surfaces {
            for (lane, counter) in lanes {
                findings.push(Finding::info(format!(
                    "{surface}: {lane} fired {} time(s), last at unix {}",
                    counter.count, counter.last_unix_secs,
                )));
            }
        }
    }
    SectionReport {
        section: "resolver_compat",
        findings,
    }
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
                    // P3-C planning states. Typed here rather than falling
                    // through to the generic warn arm so an operator sees the
                    // ONE action each admits, and so `empty_root_refused`
                    // reads as the deliberate refusal it is rather than as an
                    // unexplained warning.
                    "source_unavailable" => Finding::warn(format!(
                        "project `{}` has no usable code source this pass: {}",
                        record.project_id, record.diagnostic
                    ))
                    .with_next(
                        "attach a checkout, or publish a collected generation, to give the project a source",
                    ),
                    "empty_root_refused" => Finding::action(
                        format!(
                            "project `{}` local scan returned zero entries and the purge was refused: {}",
                            record.project_id, record.diagnostic
                        ),
                        "restore the checkout, or acknowledge the empty root with `bbox_reindex accept_empty_projects`",
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
                    let scope = state
                        .checkout_access
                        .acquire(request(CheckoutAccessKind::PublisherConfigTreeRead, None));
                    let git = scope.and_then(|scope| {
                        state.checkout_access.acquire(request(
                            CheckoutAccessKind::GitHistory,
                            scope.published_scope().cloned(),
                        ))
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

/// The five-state repo-history health model, rendered beside the code-source
/// findings (Phase 3 plan section 10 item 5).
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
        HISTORY_REFRESH_FAILED_CODE, HistoryHealthInputsV1, HistoryHealthStateV1,
        derive_history_health,
    };

    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Vec::new();
    };
    let Ok(pinned) = catalog_store.snapshot() else {
        return vec![Finding::blocked(
            "the project catalog is unreadable, so repository-history health cannot be derived",
        )];
    };
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let overlays =
        bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir).unwrap_or_default();
    let failed_refreshes = state
        .code_sources
        .store()
        .health_records()
        .unwrap_or_default()
        .into_iter()
        .filter(|record| record.code == HISTORY_REFRESH_FAILED_CODE)
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
    let mut findings = match crate::embed_runtime::status_response_for_state(state) {
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
    match bbox_vectors::try_metrics() {
        None => findings.push(Finding::warn(
            "vector connectivity diagnostics unavailable: store is warming up",
        )),
        Some(metrics) => {
            let routes = metrics.into_keys().take(64).collect::<Vec<_>>();
            match bbox_vectors::try_diagnostics_bounded(
                &routes,
                std::time::Duration::from_millis(2_000),
            ) {
                None => findings.push(Finding::warn(
                    "vector connectivity diagnostics unavailable: store is warming up",
                )),
                Some(Err(err)) => findings.push(Finding::warn(format!(
                    "vector connectivity diagnostics unavailable: {err:#}"
                ))),
                Some(Ok(report)) => {
                    for unavailable in report.unavailable {
                        findings.push(Finding::warn(format!(
                            "vector connectivity unknown for {}: {}",
                            unavailable.route,
                            unavailable.reason.as_str()
                        )));
                    }
                    let mut checked = 0usize;
                    for metrics in report.partitions.into_values() {
                        let Some(hnsw) = metrics.hnsw else {
                            continue;
                        };
                        checked += 1;
                        if hnsw.connectivity_breach(bbox_vectors::NOTIFY_CONNECTIVITY_RATIO) {
                            findings.push(Finding::action(
                                format!(
                                    "vector connectivity degraded for {}: {:.2}% zero-in-degree",
                                    metrics.route,
                                    hnsw.connectivity_risk_ratio() * 100.0
                                ),
                                "run the embed-compaction-arc workflow to rebuild the partition",
                            ));
                        }
                    }
                    if checked > 0
                        && !findings.iter().any(|finding| {
                            finding.message.starts_with("vector connectivity degraded")
                                || finding.message.starts_with("vector connectivity unknown")
                        })
                    {
                        findings.push(Finding::ok(format!(
                            "HNSW connectivity diagnostics healthy across {checked} partition(s)"
                        )));
                    }
                }
            }
        }
    }
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
    let records = state.records_provider.records_snapshot().records;
    let mut findings = Vec::new();
    let mut present = 0usize;
    for record in records.iter() {
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
                "graph",
                "projects",
                "checkout_access",
                "resolver_compat",
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
}
