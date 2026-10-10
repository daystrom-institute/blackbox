//! `/system`: daemon health. Each panel reads through the same internal
//! function as its tool or admin route, on its own blocking task, and renders
//! its own region: a failing or panicking reader shows the shared error
//! fragment in its panel and every other panel still renders.
//! `/fragments/system/{panel}` serves the same renderer alone, and each panel
//! refreshes itself through it.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use bbox_gaps::gaps::GapResolution;
use maud::{Markup, html};

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::readouts::{bytes, flatten_json, kv_table, serde_label, source, span, state_label};
use crate::doctor::FindingLevel;
use crate::orchestration;
use crate::orchestration::providers::Provider;

/// Seconds between a panel's own refreshes.
const REFRESH_SECS: u64 = 30;

/// One panel of the system page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Index,
    Doctor,
    Storage,
    Embed,
    Providers,
    Allocator,
    Gaps,
    Runtime,
}

impl Panel {
    const ALL: [Panel; 8] = [
        Panel::Doctor,
        Panel::Index,
        Panel::Storage,
        Panel::Embed,
        Panel::Providers,
        Panel::Allocator,
        Panel::Gaps,
        Panel::Runtime,
    ];

    fn slug(self) -> &'static str {
        match self {
            Panel::Index => "index",
            Panel::Doctor => "doctor",
            Panel::Storage => "storage",
            Panel::Embed => "embed",
            Panel::Providers => "providers",
            Panel::Allocator => "allocator",
            Panel::Gaps => "gaps",
            Panel::Runtime => "runtime",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Panel::Index => "Index",
            Panel::Doctor => "Doctor",
            Panel::Storage => "Storage and GC",
            Panel::Embed => "Embedding routes",
            Panel::Providers => "Providers",
            Panel::Allocator => "Allocator",
            Panel::Gaps => "Open gaps",
            Panel::Runtime => "Runtime metrics",
        }
    }

    /// The reader an error fragment names.
    fn reader(self) -> &'static str {
        match self {
            Panel::Index => "bbox_stats",
            Panel::Doctor => "bbox_doctor",
            Panel::Storage => "bbox_storage_health",
            Panel::Embed => "bbox_embed_status",
            Panel::Providers => "bro_providers",
            Panel::Allocator => "bro_allocator_status",
            Panel::Gaps => "bbox_gaps",
            Panel::Runtime => "/admin/runtime-metrics",
        }
    }

    fn parse(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|panel| panel.slug() == slug)
    }
}

/// `GET /system`
pub(crate) async fn page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    // Every panel reads in parallel on its own blocking task.
    let pending: Vec<_> = Panel::ALL
        .into_iter()
        .map(|panel| (panel, spawn_panel(ui.clone(), panel)))
        .collect();
    let mut panels = Vec::with_capacity(pending.len());
    for (panel, task) in pending {
        panels.push(finish_panel(panel, task).await);
    }
    let content = html! {
        div.panels.system-panels { @for panel in panels { (panel) } }
    };
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(&principal),
            Some(Section::System),
            "System",
            content,
        ),
    )
}

/// `GET /fragments/system/{panel}`
pub(crate) async fn fragment(State(ui): State<Arc<UiState>>, Path(slug): Path<String>) -> Response {
    match Panel::parse(&slug) {
        Some(panel) => {
            let task = spawn_panel(ui, panel);
            html_response(StatusCode::OK, finish_panel(panel, task).await)
        }
        None => html_response(
            StatusCode::NOT_FOUND,
            empty_state("No system panel has this name.", "/system"),
        ),
    }
}

fn spawn_panel(ui: Arc<UiState>, panel: Panel) -> tokio::task::JoinHandle<Markup> {
    tokio::task::spawn_blocking(move || render_body(&ui, panel))
}

async fn finish_panel(panel: Panel, task: tokio::task::JoinHandle<Markup>) -> Markup {
    let body = match task.await {
        Ok(body) => body,
        Err(error) => error_fragment(panel.reader(), &error.to_string()),
    };
    let id = format!("system-{}", panel.slug());
    html! {
        section.panel id=(id) aria-labelledby=(format!("{id}-title"))
            hx-get=(format!("/fragments/system/{}", panel.slug()))
            hx-trigger=(format!("every {REFRESH_SECS}s"))
            hx-swap="outerHTML" {
            h2 id=(format!("{id}-title")) { (panel.title()) }
            (body)
        }
    }
}

fn render_body(ui: &UiState, panel: Panel) -> Markup {
    match panel {
        Panel::Index => index_body(ui),
        Panel::Doctor => doctor_body(ui),
        Panel::Storage => storage_body(ui),
        Panel::Embed => embed_body(ui),
        Panel::Providers => providers_body(),
        Panel::Allocator => allocator_body(ui),
        Panel::Gaps => gaps_body(ui),
        Panel::Runtime => runtime_body(),
    }
}

fn index_body(ui: &UiState) -> Markup {
    match ui.shared.idx.read().stats() {
        Ok(text) => {
            let rows = super::readouts::stats_rows(&text);
            if rows.is_empty() {
                return empty_state("The index reported no statistics.", "bbox_stats");
            }
            html! {
                (kv_table(rows.into_iter().map(|(key, value)| (key, html! { (value) })).collect()))
                (source("bbox_stats"))
            }
        }
        Err(error) => error_fragment("bbox_stats", &format!("{error:#}")),
    }
}

/// A doctor level as a state verdict.
fn level_label(level: FindingLevel) -> Markup {
    let class = match level {
        FindingLevel::Ok => "running",
        FindingLevel::Info => "finished",
        FindingLevel::Warn => "waiting",
        FindingLevel::Action => "stale",
        FindingLevel::Blocked => "failed",
    };
    state_label(class, level.as_str())
}

fn doctor_body(ui: &UiState) -> Markup {
    let report = match crate::doctor::run(&ui.server) {
        Ok(report) => report,
        Err(error) => return error_fragment("bbox_doctor", &format!("{error:#}")),
    };
    let mut findings: Vec<_> = report
        .sections
        .iter()
        .flat_map(|section| {
            section
                .findings
                .iter()
                .filter(|finding| finding.level != FindingLevel::Ok)
                .map(move |finding| (section.section, finding))
        })
        .collect();
    findings.sort_by(|(a_section, a), (b_section, b)| {
        b.level
            .cmp(&a.level)
            .then_with(|| a_section.cmp(b_section))
            .then_with(|| a.message.cmp(&b.message))
    });
    html! {
        p { "Overall " (level_label(report.status)) }
        table {
            thead { tr { th scope="col" { "Section" } th scope="col" { "Status" } } }
            tbody {
                @for section in &report.sections {
                    tr { td { (section.section) } td { (level_label(section.worst())) } }
                }
            }
        }
        @if findings.is_empty() {
            (empty_state("Every doctor check passed.", "bbox_doctor"))
        } @else {
            h3 { "Findings" }
            table {
                thead {
                    tr {
                        th scope="col" { "Level" }
                        th scope="col" { "Section" }
                        th scope="col" { "Finding" }
                    }
                }
                tbody {
                    @for (section, finding) in &findings {
                        tr {
                            td { (level_label(finding.level)) }
                            td { (section) }
                            td {
                                (finding.message)
                                @if let Some(next) = &finding.next {
                                    br; span.finding-next { "Next: " code { (next) } }
                                }
                            }
                        }
                    }
                }
            }
        }
        (source("bbox_doctor"))
    }
}

fn storage_body(ui: &UiState) -> Markup {
    let schedule = crate::server::storage_gc::storage_gc_schedule();
    let gc = kv_table(vec![
        (
            "Pass interval".to_string(),
            html! { (span(schedule.interval.as_secs())) },
        ),
        (
            "First pass after start".to_string(),
            html! { (span(schedule.initial_delay.as_secs())) },
        ),
        (
            "Inactive snapshot age limit".to_string(),
            html! {
                @if schedule.snapshot_max_age_days == 0 { "disabled" }
                @else { (schedule.snapshot_max_age_days) "d" }
            },
        ),
    ]);
    let health = match ui.server.storage_health_report(None, false) {
        Ok((report, _)) => {
            let totals = &report.totals;
            let categories = [
                (
                    "active legacy",
                    totals.active_legacy_files,
                    totals.active_legacy_bytes,
                ),
                (
                    "managed derived",
                    totals.managed_derived_files,
                    totals.managed_derived_bytes,
                ),
                (
                    "inactive snapshots",
                    totals.inactive_snapshot_files,
                    totals.inactive_snapshot_bytes,
                ),
                ("backups", totals.backup_files, totals.backup_bytes),
                ("temporary", totals.temp_files, totals.temp_bytes),
                ("orphans", totals.orphan_files, totals.orphan_bytes),
                ("observed", totals.observed_files, totals.observed_bytes),
            ];
            html! {
                table {
                    thead {
                        tr {
                            th scope="col" { "Category" }
                            th.num scope="col" { "Files" }
                            th.num scope="col" { "Size" }
                        }
                    }
                    tbody {
                        @for (label, files, size) in categories {
                            tr { td { (label) } td.num { (files) } td.num { (bytes(size)) } }
                        }
                    }
                }
                @if let Some(manifest) = &report.manifest_status {
                    p {
                        "Manifest "
                        @if manifest.index_exists { (state_label("running", &manifest.status)) }
                        @else { (state_label("waiting", &manifest.status)) }
                        ", " (manifest.workspace_count) " workspaces"
                        @if let Some(reason) = &manifest.fallback_reason { " (" (reason) ")" }
                    }
                }
                @if let Some(warning) = &report.observed_policy_warning {
                    p { (state_label("waiting", "warning")) " " (warning) }
                }
                @if !report.top_offenders.is_empty() {
                    h3 { "Largest contributors" }
                    table {
                        thead {
                            tr {
                                th scope="col" { "Kind" }
                                th scope="col" { "Project" }
                                th.num scope="col" { "Size" }
                            }
                        }
                        tbody {
                            @for file in &report.top_offenders {
                                tr {
                                    td { (serde_label(&file.kind)) }
                                    td { (file.project_id.as_deref().unwrap_or("none")) }
                                    td.num { (bytes(file.bytes)) }
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(error) => error_fragment("bbox_storage_health", &format!("{error:#}")),
    };
    html! {
        (health)
        h3 { "GC schedule" }
        (gc)
        (source("bbox_storage_health, storage GC maintenance pass"))
    }
}

fn embed_body(ui: &UiState) -> Markup {
    let response = match crate::embed_runtime::status_response_for_state(&ui.shared, false) {
        Ok(response) => response,
        Err(error) => return error_fragment("bbox_embed_status", &format!("{error:#}")),
    };
    if response.routes.is_empty() {
        return empty_state("No embedding routes are active.", "bbox_embed_status");
    }
    html! {
        table {
            thead {
                tr {
                    th scope="col" { "Route" }
                    th scope="col" { "Health" }
                    th scope="col" { "Model" }
                    th.num scope="col" { "Indexed" }
                    th.num scope="col" { "Queued" }
                    th.num scope="col" { "Queue size" }
                    th.num scope="col" { "Retried" }
                    th.num scope="col" { "Dropped" }
                }
            }
            tbody {
                @for (route, status) in &response.routes {
                    @let class = match (status.available, status.health.as_str()) {
                        (true, "ok") => "running",
                        (false, _) => "failed",
                        _ => "waiting",
                    };
                    tr {
                        td {
                            code { (route) }
                            @if let Some(error) = &status.last_error {
                                br; span.finding-next { (error) }
                            }
                        }
                        td { (state_label(class, &status.health)) }
                        td {
                            (status.provider.as_deref().unwrap_or("unset")) " "
                            code { (status.model.as_deref().unwrap_or("unset")) }
                        }
                        td.num { (status.indexed_count) }
                        td.num { (status.queue_depth) }
                        td.num { (bytes(status.queue_bytes)) }
                        td.num { (status.retried_count) }
                        td.num { (status.dropped_count) }
                    }
                }
            }
        }
        (source("bbox_embed_status"))
    }
}

fn providers_body() -> Markup {
    let now = orchestration::now_ms();
    html! {
        table {
            thead {
                tr {
                    th scope="col" { "Provider" }
                    th scope="col" { "Prompt cache" }
                    th scope="col" { "Resume" }
                    th.num scope="col" { "Models" }
                    th scope="col" { "Default model" }
                    th scope="col" { "Peak usage" }
                }
            }
            tbody {
                @for provider in Provider::ALL {
                    tr {
                        td { (provider.as_str()) }
                        td { (serde_label(&provider.prompt_cache())) }
                        td { @if provider.supports_resume() { "yes" } @else { "no" } }
                        td.num { (provider.models().len()) }
                        td {
                            @match provider.models().iter().find(|model| model.default) {
                                Some(model) => { code { (model.id) } }
                                None => { "none" }
                            }
                        }
                        td {
                            @match orchestration::usage_schedule::peak_usage(*provider, now) {
                                Some(true) => { (state_label("waiting", "peak")) }
                                Some(false) => { (state_label("running", "off-peak")) }
                                None => { "no schedule" }
                            }
                        }
                    }
                }
            }
        }
        (source("bro_providers"))
    }
}

fn allocator_body(ui: &UiState) -> Markup {
    let store_dir = &ui.shared.store_dir;
    let config = orchestration::allocator::load_effective_config(store_dir, None);
    let leases = orchestration::allocator::lease_store_load(store_dir);
    let probes = match orchestration::allocator::probe_store_read(store_dir) {
        Ok(probes) => probes,
        Err(error) => return error_fragment("bro_allocator_status", &format!("{error:#}")),
    };
    let context = orchestration::allocator::allocation_context_with_probes(
        &ui.shared.task_store.read(),
        &leases,
        probes.clone(),
    );
    let now = orchestration::now_ms();
    html! {
        (kv_table(vec![
            ("Pools".to_string(), html! { (config.pools.len()) }),
            ("Tiers".to_string(), html! { (config.tiers.len()) }),
            ("Selection policies".to_string(), html! { (config.selection_policies.len()) }),
            ("Runtime leases".to_string(), html! { (leases.leases.len()) }),
        ]))
        h3 { "In flight" }
        @if context.in_flight.is_empty() {
            (empty_state("No lane has a task in flight.", "bro_allocator_status"))
        } @else {
            table {
                thead { tr { th scope="col" { "Lane" } th.num scope="col" { "Tasks" } } }
                tbody {
                    @for (lane, count) in &context.in_flight {
                        tr { td { code { (lane) } } td.num { (count) } }
                    }
                }
            }
        }
        h3 { "Lane probes" }
        @if probes.records.is_empty() {
            (empty_state("No lane has been probed.", "bro_allocator_status"))
        } @else {
            table {
                thead {
                    tr {
                        th scope="col" { "Lane" }
                        th scope="col" { "Credential" }
                        th scope="col" { "Quota" }
                        th scope="col" { "Cooldown" }
                    }
                }
                tbody {
                    @for (lane, record) in &probes.records {
                        @let credential = serde_label(&record.credential_status);
                        @let credential_class = match credential.as_str() {
                            "present" => "running",
                            "missing" | "expired" => "failed",
                            _ => "stale",
                        };
                        @let quota = serde_label(&record.quota_status);
                        @let quota_class = match quota.as_str() {
                            "known" => "running",
                            "exhausted" | "probe_failed" => "failed",
                            _ => "stale",
                        };
                        tr {
                            td { code { (lane) } }
                            td { (state_label(credential_class, &credential)) }
                            td { (state_label(quota_class, &quota)) }
                            td {
                                @match record.cooldown_until {
                                    Some(until) if until > now => { (span((until - now) / 1000)) " left" }
                                    _ => { "none" }
                                }
                            }
                        }
                    }
                }
            }
        }
        (source("bro_allocator_status"))
    }
}

fn gaps_body(ui: &UiState) -> Markup {
    let view = match ui.server.session_gap_view(None) {
        Ok(view) => view,
        Err(error) => return error_fragment("bbox_gaps", &format!("{error:#}")),
    };
    let all = view.gaps.all();
    let count = |resolution: GapResolution| {
        all.iter()
            .filter(|gap| gap.resolution == resolution)
            .count()
    };
    let unresolved = count(GapResolution::Unresolved);
    let acknowledged = count(GapResolution::Acknowledged);
    html! {
        @if unresolved + acknowledged == 0 {
            (empty_state("No gaps are open.", "bbox_gaps"))
        } @else {
            table {
                thead { tr { th scope="col" { "Resolution" } th.num scope="col" { "Count" } } }
                tbody {
                    tr { td { (state_label("waiting", "unresolved")) } td.num { (unresolved) } }
                    tr { td { (state_label("stale", "acknowledged")) } td.num { (acknowledged) } }
                }
            }
            (source("bbox_gaps"))
        }
    }
}

fn runtime_body() -> Markup {
    let snapshot = crate::server::runtime_metrics::latest_runtime_metrics_snapshot();
    let latency = crate::server::runtime_metrics::scheduler_latency_snapshot();
    html! {
        h3 { "Runtime sample" }
        @match &snapshot {
            Some(snapshot) => {
                (kv_table(flatten_json("", snapshot).into_iter().map(|(key, value)| (key, html! { (value) })).collect()))
            }
            None => {
                (empty_state("No runtime metrics sample has been published yet.", "/admin/runtime-metrics"))
            }
        }
        h3 { "Scheduler latency" }
        (kv_table(flatten_json("", &latency).into_iter().map(|(key, value)| (key, html! { (value) })).collect()))
        (source("/admin/runtime-metrics"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;

    use super::super::readouts::test_support::get;
    use super::Panel;
    use crate::server::SharedState;

    #[tokio::test]
    async fn system_page_renders_every_panel_as_its_own_refreshing_region() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (status, body) = get(shared, "/system").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("<h1>System</h1>"), "{body}");
        assert!(body.contains("href=\"/system\" aria-current=\"page\""));
        for panel in Panel::ALL {
            assert!(
                body.contains(&format!("id=\"system-{}\"", panel.slug())),
                "missing panel {}: {body}",
                panel.slug()
            );
            assert!(body.contains(&format!("hx-get=\"/fragments/system/{}\"", panel.slug())));
            assert!(body.contains(panel.title()));
        }
        assert!(body.contains("Pass interval"), "{body}");
        assert!(body.contains("Scheduler latency"), "{body}");
        assert!(
            body.contains("Overall <span class=\"state state-"),
            "{body}"
        );
        for provider in crate::orchestration::providers::Provider::ALL {
            assert!(body.contains(provider.as_str()));
        }
    }

    #[tokio::test]
    async fn a_panel_fragment_is_the_same_region_alone_and_an_unknown_panel_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        for panel in Panel::ALL {
            let (status, body) = get(
                shared.clone(),
                &format!("/fragments/system/{}", panel.slug()),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{}: {body}", panel.slug());
            assert!(
                body.starts_with(&format!(
                    "<section class=\"panel\" id=\"system-{}\"",
                    panel.slug()
                )),
                "{body}"
            );
            assert!(!body.contains("<html"));
        }
        let (status, _) = get(shared, "/fragments/system/nothing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
