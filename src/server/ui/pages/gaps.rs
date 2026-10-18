//! `/gaps` and `/gaps/{id}`: the gap list with resolution, kind, impact,
//! blocking, domain, project and text filters, and one gap's full record
//! with its origin, provenance, resolution and related gaps.
//!
//! Both read through `BlackboxServer::resolve_gap_list`, the selection
//! `bbox_gaps` makes: the same session gap view, the same project identity
//! and ledger arms, and the same diagnostics. The list table refreshes in
//! place on an interval; the daemon keeps no gap broadcast to stream from.
//!
//! The project filter offers one choice per project the stored gaps name,
//! built the way the threads filter builds its choices; the selected value
//! is handed to the tool's own project filter.
//!
//! The panels other pages show (gaps raised by a thread, a task, or open in
//! a project) are rendered here too, so every gap row links the same way.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use bbox_corpus_core::built_from::BuiltFromStamp;
use bbox_gaps::gaps::{BlockingLevel, GapImpact, GapKind, GapListParams, GapNote, GapResolution};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;
use strum::VariantNames;

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::display::{age, encode_component, parse_rfc3339, preview};
use super::readouts::state_label;
use super::threads::{ProjectChoice, ProjectRef, project_choices, select_choice};
use crate::server::gap_view::SessionGapView;
use crate::server::knowledge_view::LEGACY_COMPATIBILITY_LANE;
use crate::tools::gaps::gap_diagnostic_preview;

/// Rows per list page.
const PAGE_SIZE: usize = 100;

/// Rows a related-gaps panel on another page shows before linking to the
/// filtered list.
const RELATED_ROWS: usize = 10;

/// How often the list table re-fetches itself.
const REFRESH_TRIGGER: &str = "every 30s";

/// Resolution filter choices, in display order, as `(value, label)`. `open`
/// (the default) lists unresolved and acknowledged gaps; `all` adds
/// addressed ones; the rest select one.
const RESOLUTION_CHOICES: [(&str, &str); 5] = [
    ("open", "unresolved and acknowledged"),
    ("all", "all resolutions"),
    ("unresolved", "unresolved"),
    ("acknowledged", "acknowledged"),
    ("addressed", "addressed"),
];

/// Impact levels, most severe first.
static IMPACT_ORDER: [GapImpact; 4] = [
    GapImpact::Critical,
    GapImpact::High,
    GapImpact::Medium,
    GapImpact::Low,
];

/// The list's query string: plain GET parameters, so filtering works
/// without JavaScript.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ListQuery {
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    impact: Option<String>,
    #[serde(default)]
    blocking: Option<String>,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
}

fn given(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

impl ListQuery {
    /// The resolution value that names the selection, absent for the
    /// default (`open`).
    fn resolution(&self) -> Option<&str> {
        given(&self.resolution).filter(|resolution| *resolution != "open")
    }

    fn project(&self) -> Option<&str> {
        given(&self.project)
    }

    fn offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }

    fn is_filtered(&self) -> bool {
        self.resolution().is_some()
            || given(&self.kind).is_some()
            || given(&self.impact).is_some()
            || given(&self.blocking).is_some()
            || given(&self.domain).is_some()
            || self.project().is_some()
            || given(&self.q).is_some()
    }

    /// `base` with these filters at `offset`, `impact` replaced when given.
    fn href_with(&self, base: &str, offset: usize, impact: Option<&str>) -> String {
        let mut params = Vec::new();
        let mut push = |name: &str, value: Option<&str>| {
            if let Some(value) = value {
                params.push(format!("{name}={}", encode_component(value)));
            }
        };
        push("resolution", self.resolution());
        push("kind", given(&self.kind));
        push("impact", impact.or_else(|| given(&self.impact)));
        push("blocking", given(&self.blocking));
        push("domain", given(&self.domain));
        push("project", self.project());
        push("q", given(&self.q));
        if offset > 0 {
            params.push(format!("offset={offset}"));
        }
        if params.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", params.join("&"))
        }
    }

    fn href(&self, offset: usize) -> String {
        self.href_with("/gaps", offset, None)
    }

    /// The tool selection these filters make, before project resolution.
    fn params(&self) -> GapListParams {
        let (resolution, include_addressed) = match self.resolution() {
            None => (None, false),
            Some("all") => (None, true),
            Some(resolution) => (Some(resolution.to_string()), true),
        };
        GapListParams {
            resolution,
            include_addressed: Some(include_addressed),
            gap_kind: given(&self.kind).map(str::to_string),
            impact: given(&self.impact).map(str::to_string),
            blocking_level: given(&self.blocking).map(str::to_string),
            domain: given(&self.domain).map(str::to_string),
            project: self.project().map(str::to_string),
            query: given(&self.q).map(str::to_string),
            ..GapListParams::default()
        }
    }
}

/// The stored wire name of a gap enum (`mcp_surface`, `blocks_task`, ...).
fn wire<T: AsRef<str>>(value: &T) -> &str {
    value.as_ref()
}

/// The state class a resolution renders under, matching the overview.
fn resolution_state(resolution: GapResolution) -> &'static str {
    match resolution {
        GapResolution::Unresolved => "waiting",
        GapResolution::Acknowledged => "stale",
        GapResolution::Addressed => "finished",
    }
}

fn resolution_label(resolution: GapResolution) -> Markup {
    state_label(resolution_state(resolution), wire(&resolution))
}

/// The key a gap is stored under: its project id when stamped, else the
/// stored selector. A gap with neither is global.
fn gap_project(gap: &GapNote) -> Option<&str> {
    gap.project_id.as_deref().or_else(|| {
        gap.project
            .as_deref()
            .filter(|project| !project.trim().is_empty())
    })
}

/// Where a row in the view came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Provenance {
    /// Read from a project's published gap files at this ref and commit.
    Published { reference: String, commit: String },
    /// A central-store row with no provable published stamp.
    Legacy,
    /// Neither stamp nor lane label; a row read before either existed.
    Unstamped,
}

fn provenance(view: &SessionGapView, id: &str) -> Provenance {
    let Some(metadata) = view.gaps.view_metadata(id) else {
        return Provenance::Unstamped;
    };
    if let Some(BuiltFromStamp::Published {
        published_ref,
        publisher_commit,
        ..
    }) = metadata
        .built_from_ref
        .as_deref()
        .and_then(|reference| view.built_from.get(reference))
    {
        return Provenance::Published {
            reference: published_ref.clone(),
            commit: publisher_commit.clone(),
        };
    }
    if metadata.compatibility_lane.as_deref() == Some(LEGACY_COMPATIBILITY_LANE) {
        Provenance::Legacy
    } else {
        Provenance::Unstamped
    }
}

fn short_commit(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

fn provenance_cell(provenance: &Provenance) -> Markup {
    html! {
        @match provenance {
            Provenance::Published { reference, commit } => {
                "published " code { (reference) } " at " code title=(commit) { (short_commit(commit)) }
            }
            Provenance::Legacy => { span.badge { "legacy" } " central store, no published stamp" }
            Provenance::Unstamped => { span.quiet { "unstamped" } }
        }
    }
}

/// One piece of free text: plain text or an http(s) URL.
#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    Text(&'a str),
    Url(&'a str),
}

/// Split `text` around its http(s) URLs. A URL ends at whitespace or a
/// closing bracket or quote, and trailing sentence punctuation stays text.
fn segments(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| rest.find(scheme))
        .min()
    {
        let tail = &rest[start..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '<'))
            .unwrap_or(tail.len());
        while end > 0 && tail[..end].ends_with(['.', ',', ';', ':', '!', '?']) {
            end -= 1;
        }
        let scheme_len = if tail.starts_with("https://") { 8 } else { 7 };
        if end <= scheme_len {
            out.push(Segment::Text(&rest[..start + scheme_len]));
            rest = &rest[start + scheme_len..];
            continue;
        }
        if start > 0 {
            out.push(Segment::Text(&rest[..start]));
        }
        out.push(Segment::Url(&tail[..end]));
        rest = &tail[end..];
    }
    if !rest.is_empty() {
        out.push(Segment::Text(rest));
    }
    out
}

/// Free text with its URLs as links.
fn linkified(text: &str) -> Markup {
    html! {
        @for segment in segments(text) {
            @match segment {
                Segment::Text(text) => { (text) }
                Segment::Url(url) => { a href=(url) rel="noreferrer" { (url) } }
            }
        }
    }
}

/// A GitHub issue or pull request a gap's resolution note links to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IssueLink {
    label: String,
    url: String,
}

/// The GitHub issues and pull requests `text` links to, in order, once each.
fn issue_links(text: &str) -> Vec<IssueLink> {
    let mut seen = BTreeSet::new();
    segments(text)
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Url(url) => Some(url),
            Segment::Text(_) => None,
        })
        .filter_map(|url| {
            let path = url
                .strip_prefix("https://github.com/")
                .or_else(|| url.strip_prefix("http://github.com/"))?;
            let mut parts = path.split(['/', '#', '?']);
            let (_owner, repo, kind, number) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            let prefix = match kind {
                "issues" => "",
                "pull" => "PR ",
                _ => return None,
            };
            (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then(|| {
                IssueLink {
                    label: format!("{prefix}{repo}#{number}"),
                    url: url.to_string(),
                }
            })
        })
        .filter(|link| seen.insert(link.url.clone()))
        .collect()
}

fn gap_issues(gap: &GapNote) -> Vec<IssueLink> {
    gap.resolution_note
        .as_deref()
        .map(issue_links)
        .unwrap_or_default()
}

fn issue_cell(issues: &[IssueLink]) -> Markup {
    html! {
        @if issues.is_empty() {
            span.quiet { "none" }
        } @else {
            @for (index, issue) in issues.iter().enumerate() {
                @if index > 0 { ", " }
                a href=(issue.url) rel="noreferrer" title=(issue.url) { (issue.label) }
            }
        }
    }
}

fn gap_href(id: &str) -> String {
    format!("/gaps/{}", encode_component(id))
}

/// A gap's project: the name linking to the gaps it filters, with the
/// stored key on hover. A gap with no project is global.
fn project_cell(project: Option<&ProjectRef>) -> Markup {
    html! {
        @match project {
            Some(project) => {
                a href=(format!("/gaps?project={}", encode_component(&project.value)))
                    title=(project.key) {
                    (preview(&project.label, 60))
                }
            }
            None => { span.quiet { "global" } }
        }
    }
}

/// The project reference of one stored key, labelled the way the filter
/// labels it.
fn project_ref(ui: &UiState, key: &str) -> ProjectRef {
    let (choices, _) = project_choices(ui, &BTreeSet::from([key.to_string()]), None);
    choices
        .into_iter()
        .find(|choice| choice.keys.contains(key))
        .map(|choice| choice.reference(key))
        .unwrap_or_else(|| ProjectRef {
            value: key.to_string(),
            label: key.to_string(),
            project_id: None,
            key: key.to_string(),
        })
}

struct GapRow {
    gap: GapNote,
    project: Option<ProjectRef>,
    provenance: Provenance,
    issues: Vec<IssueLink>,
}

struct ListData {
    rows: Vec<GapRow>,
    total: usize,
    /// Matching gaps per impact level, most severe first.
    impacts: Vec<(GapImpact, usize)>,
    /// The filter choices: one per project a stored gap names.
    projects: Vec<ProjectChoice>,
    /// The value of the choice the query selected, when it named one.
    selected: Option<String>,
    /// Every domain a stored gap names, sorted.
    domains: Vec<String>,
    /// Whether the view holds any gap at all.
    store_empty: bool,
    /// The view's diagnostics, as `bbox_gaps(debug=true)` previews them.
    diagnostics: Vec<String>,
}

/// `bbox_gaps` reader: the session gap view under the query's filters,
/// newest first.
fn read_list(ui: &UiState, query: &ListQuery) -> anyhow::Result<ListData> {
    let (mut view, params, filter_diagnostics) = ui.server.resolve_gap_list(query.params())?;
    let all = view.gaps.all();
    let store_empty = all.is_empty();
    let keys: BTreeSet<String> = all
        .iter()
        .filter_map(|gap| gap_project(gap).map(str::to_string))
        .collect();
    let domains: Vec<String> = all
        .iter()
        .map(|gap| gap.domain.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (choices, resolved) = project_choices(ui, &keys, query.project());
    let selected = query
        .project()
        .and_then(|wanted| select_choice(&choices, &resolved, wanted))
        .map(|choice| choice.value.clone());

    let matched = view.gaps.select(&params)?;
    let total = matched.len();
    let impacts = IMPACT_ORDER
        .iter()
        .map(|impact| {
            let count = matched.iter().filter(|gap| gap.impact == *impact).count();
            (*impact, count)
        })
        .collect();
    let page: Vec<GapNote> = matched
        .into_iter()
        .skip(query.offset())
        .take(PAGE_SIZE)
        .collect();
    let returned_legacy =
        view.returned_rows_include_legacy_lane(page.iter().map(|gap| gap.id.as_str()));
    let rows = page
        .into_iter()
        .map(|gap| {
            let project = gap_project(&gap).map(|key| {
                choices
                    .iter()
                    .find(|choice| choice.keys.contains(key))
                    .map(|choice| choice.reference(key))
                    .unwrap_or_else(|| ProjectRef {
                        value: key.to_string(),
                        label: key.to_string(),
                        project_id: None,
                        key: key.to_string(),
                    })
            });
            GapRow {
                provenance: provenance(&view, &gap.id),
                issues: gap_issues(&gap),
                project,
                gap,
            }
        })
        .collect();
    view.finalize_response_diagnostics(returned_legacy, filter_diagnostics);
    Ok(ListData {
        rows,
        total,
        impacts,
        projects: choices,
        selected,
        domains,
        store_empty,
        diagnostics: gap_diagnostic_preview(&view.diagnostics, true),
    })
}

async fn list_data(ui: &Arc<UiState>, query: ListQuery) -> (ListQuery, Result<ListData, String>) {
    let reader_ui = ui.clone();
    match tokio::task::spawn_blocking(move || {
        let data = read_list(&reader_ui, &query).map_err(|error| format!("{error:#}"));
        (query, data)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => (ListQuery::default(), Err(error.to_string())),
    }
}

/// `GET /gaps`
pub(crate) async fn list_page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ListQuery>,
) -> Response {
    let (query, data) = list_data(&ui, query).await;
    let content = html! {
        @if let Ok(data) = &data {
            (filter_form(&query, data))
        }
        div id="gaps-table" hx-get=(query.href_with("/fragments/gaps/table", query.offset(), None))
            hx-trigger=(REFRESH_TRIGGER) hx-swap="innerHTML" {
            (list_table(&query, &data, Utc::now()))
        }
    };
    html_response(
        StatusCode::OK,
        layout(&ui.shell(&principal), Some(Section::Gaps), "Gaps", content),
    )
}

/// `GET /fragments/gaps/table`: the list table the page renders.
pub(crate) async fn table_fragment(
    State(ui): State<Arc<UiState>>,
    Query(query): Query<ListQuery>,
) -> Response {
    let (query, data) = list_data(&ui, query).await;
    html_response(StatusCode::OK, list_table(&query, &data, Utc::now()))
}

fn choice_select(
    name: &str,
    label: &str,
    all: &str,
    choices: &[&str],
    current: Option<&str>,
) -> Markup {
    html! {
        label {
            (label) " "
            select name=(name) {
                option value="" selected[current.is_none()] { (all) }
                @for choice in choices {
                    option value=(choice) selected[current == Some(*choice)] { (choice) }
                }
            }
        }
    }
}

fn filter_form(query: &ListQuery, data: &ListData) -> Markup {
    let resolution = query.resolution().unwrap_or("open");
    let kinds: Vec<&str> = GapKind::VARIANTS.to_vec();
    let impacts: Vec<&str> = IMPACT_ORDER.iter().map(wire).collect();
    let blocking: Vec<&str> = BlockingLevel::VARIANTS.to_vec();
    let domains: Vec<&str> = data.domains.iter().map(String::as_str).collect();
    let current_project = data.selected.as_deref();
    html! {
        form.filters method="get" action="/gaps" {
            label {
                "Resolution "
                select name="resolution" {
                    @for (choice, label) in RESOLUTION_CHOICES {
                        option value=(choice) selected[choice == resolution] { (label) }
                    }
                }
            }
            (choice_select("kind", "Kind", "all kinds", &kinds, given(&query.kind)))
            (choice_select("impact", "Impact", "all impacts", &impacts, given(&query.impact)))
            (choice_select("blocking", "Blocking", "any blocking", &blocking, given(&query.blocking)))
            (choice_select("domain", "Domain", "all domains", &domains, given(&query.domain)))
            label {
                "Project "
                select name="project" {
                    option value="" selected[query.project().is_none()] { "all projects" }
                    @for choice in &data.projects {
                        option value=(choice.value) selected[Some(choice.value.as_str()) == current_project] {
                            (preview(&choice.label, 80))
                        }
                    }
                }
            }
            label {
                "Text "
                input type="search" name="q" value=[given(&query.q)]
                    placeholder="title, domain, capability";
            }
            button type="submit" { "Filter" }
            @if query.is_filtered() {
                a href="/gaps" { "Clear filters" }
            }
        }
    }
}

fn list_table(query: &ListQuery, data: &Result<ListData, String>, now: DateTime<Utc>) -> Markup {
    let data = match data {
        Ok(data) => data,
        Err(error) => return error_fragment("bbox_gaps", error),
    };
    let offset = query.offset();
    html! {
        @if data.total > 0 {
            p.gap-impacts {
                (data.total) " matching: "
                @for (index, (impact, count)) in data.impacts.iter().enumerate() {
                    @if index > 0 { ", " }
                    @if *count > 0 {
                        a href=(query.href_with("/gaps", 0, Some(wire(impact)))) {
                            (wire(impact)) " " span.num { (count) }
                        }
                    } @else {
                        span.quiet { (wire(impact)) " " span.num { "0" } }
                    }
                }
            }
        }
        @if data.rows.is_empty() {
            @if data.store_empty {
                (empty_state("No gaps are stored.", "bbox_gaps"))
            } @else if data.total > 0 {
                (empty_state("This page is past the last matching gap.", "bbox_gaps"))
            } @else {
                (empty_state("No gaps match these filters.", "bbox_gaps"))
            }
        } @else {
            (gap_table(&data.rows, now))
        }
        @if data.total > 0 {
            p.pager {
                @if !data.rows.is_empty() {
                    "Showing " (offset + 1) " to " (offset + data.rows.len()) " of " (data.total) ". "
                }
                @if offset > 0 {
                    a href=(query.href(offset.saturating_sub(PAGE_SIZE))) { "Newer" }
                    " "
                }
                @if offset + data.rows.len() < data.total {
                    a href=(query.href(offset + PAGE_SIZE)) { "Older" }
                }
            }
        }
        (diagnostics_block(&data.diagnostics))
        p.panel-source { "Source: " code { "bbox_gaps" } }
    }
}

fn gap_table(rows: &[GapRow], now: DateTime<Utc>) -> Markup {
    html! {
        table.gaps-table {
            thead {
                tr {
                    th scope="col" { "Title" }
                    th scope="col" { "Kind" }
                    th scope="col" { "Domain" }
                    th scope="col" { "Impact" }
                    th scope="col" { "Blocking" }
                    th scope="col" { "Resolution" }
                    th scope="col" { "Issue" }
                    th scope="col" { "Project" }
                    th scope="col" { "Created" }
                }
            }
            tbody {
                @for row in rows {
                    tr {
                        td {
                            a href=(gap_href(&row.gap.id)) { (preview(&row.gap.title, 140)) }
                            @if row.provenance == Provenance::Legacy { " " span.badge { "legacy" } }
                            div.quiet { code { (row.gap.id) } }
                        }
                        td { (wire(&row.gap.gap_kind)) }
                        td { (row.gap.domain) }
                        td { (wire(&row.gap.impact)) }
                        td { (wire(&row.gap.blocking_level)) }
                        td { (resolution_label(row.gap.resolution)) }
                        td { (issue_cell(&row.issues)) }
                        td { (project_cell(row.project.as_ref())) }
                        td { (age(parse_rfc3339(&row.gap.created_at), now)) }
                    }
                }
            }
        }
    }
}

fn diagnostics_block(diagnostics: &[String]) -> Markup {
    html! {
        @if !diagnostics.is_empty() {
            details.gap-diagnostics {
                summary { "Source diagnostics (" (diagnostics.len()) ")" }
                ul {
                    @for line in diagnostics {
                        li { code { (line) } }
                    }
                }
            }
        }
    }
}

/// Another gap a detail page names, with its title and resolution when the
/// view holds it.
struct GapLink {
    id: String,
    known: Option<(String, GapResolution)>,
}

impl GapLink {
    fn find(all: &[GapNote], id: &str) -> Self {
        Self {
            id: id.to_string(),
            known: all
                .iter()
                .find(|gap| gap.id == id)
                .map(|gap| (gap.title.clone(), gap.resolution)),
        }
    }
}

struct DetailData {
    gap: GapNote,
    project: Option<ProjectRef>,
    provenance: Provenance,
    supersedes: Option<GapLink>,
    superseded_by: Option<GapLink>,
    /// Other gaps filed under the same dedupe key, newest first.
    siblings: Vec<GapLink>,
}

/// `bbox_gaps(id)` reader: the exact record for this id, addressed
/// included.
fn read_gap(ui: &UiState, id: &str) -> anyhow::Result<Option<DetailData>> {
    let (view, params, _) = ui.server.resolve_gap_list(GapListParams {
        id: Some(id.to_string()),
        include_addressed: Some(true),
        ..GapListParams::default()
    })?;
    let Some(gap) = view.gaps.select(&params)?.into_iter().next() else {
        return Ok(None);
    };
    let all = view.gaps.all();
    let mut siblings: Vec<&GapNote> = all
        .iter()
        .filter(|other| other.dedupe_key == gap.dedupe_key && other.id != gap.id)
        .collect();
    siblings.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Some(DetailData {
        project: gap_project(&gap).map(|key| project_ref(ui, key)),
        provenance: provenance(&view, &gap.id),
        supersedes: gap.supersedes.as_deref().map(|id| GapLink::find(all, id)),
        superseded_by: gap
            .superseded_by
            .as_deref()
            .map(|id| GapLink::find(all, id)),
        siblings: siblings
            .into_iter()
            .map(|other| GapLink::find(all, &other.id))
            .collect(),
        gap,
    }))
}

async fn gap_detail(ui: &Arc<UiState>, id: String) -> Result<Option<DetailData>, String> {
    let reader_ui = ui.clone();
    match tokio::task::spawn_blocking(move || read_gap(&reader_ui, &id)).await {
        Ok(result) => result.map_err(|error| format!("{error:#}")),
        Err(error) => Err(error.to_string()),
    }
}

/// `GET /gaps/{id}`
pub(crate) async fn detail_page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let (status, title, content) = match gap_detail(&ui, id.clone()).await {
        Ok(Some(data)) => (
            StatusCode::OK,
            preview(&data.gap.title, 120),
            render_detail(&data, Utc::now()),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            "Gap not found".to_string(),
            missing_gap(&id),
        ),
        Err(error) => (
            StatusCode::OK,
            "Gap".to_string(),
            error_fragment("bbox_gaps", &error),
        ),
    };
    html_response(
        status,
        layout(&ui.shell(&principal), Some(Section::Gaps), &title, content),
    )
}

/// `GET /fragments/gaps/{id}`: the detail pane the page renders.
pub(crate) async fn detail_fragment(
    State(ui): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Response {
    match gap_detail(&ui, id.clone()).await {
        Ok(Some(data)) => html_response(StatusCode::OK, render_detail(&data, Utc::now())),
        Ok(None) => html_response(StatusCode::NOT_FOUND, missing_gap(&id)),
        Err(error) => html_response(StatusCode::OK, error_fragment("bbox_gaps", &error)),
    }
}

fn missing_gap(id: &str) -> Markup {
    html! {
        (empty_state(&format!("No gap has the id {id}."), "bbox_gaps"))
        p { a href="/gaps" { "All gaps" } }
    }
}

fn text_row(name: &str, value: Option<&str>) -> Markup {
    html! {
        tr {
            th scope="row" { (name) }
            td.prose {
                @match value.filter(|value| !value.trim().is_empty()) {
                    Some(value) => { (linkified(value)) }
                    None => { span.quiet { "none" } }
                }
            }
        }
    }
}

fn gap_link_item(link: &GapLink) -> Markup {
    html! {
        a href=(gap_href(&link.id)) { code { (link.id) } }
        @match &link.known {
            Some((title, resolution)) => {
                " " (resolution_label(*resolution)) " " (preview(title, 120))
            }
            None => { " " span.quiet { "not in this view" } }
        }
    }
}

fn render_detail(data: &DetailData, now: DateTime<Utc>) -> Markup {
    let gap = &data.gap;
    let issues = gap_issues(gap);
    let has_origin = [
        &gap.task_id,
        &gap.thread_id,
        &gap.session_id,
        &gap.provider,
        &gap.bro,
    ]
    .iter()
    .any(|value| value.is_some());
    html! {
        div.gap-detail {
            p { a href="/gaps" { "All gaps" } }
            div.panels {
                section.panel aria-labelledby="gap-summary" {
                    h2 id="gap-summary" { "Summary" }
                    table.detail-table {
                        tbody {
                            tr { th scope="row" { "Id" } td { code { (gap.id) } } }
                            tr { th scope="row" { "Resolution" } td { (resolution_label(gap.resolution)) } }
                            tr { th scope="row" { "Kind" } td { (wire(&gap.gap_kind)) } }
                            tr { th scope="row" { "Domain" } td { (gap.domain) } }
                            tr { th scope="row" { "Impact" } td { (wire(&gap.impact)) } }
                            tr { th scope="row" { "Blocking" } td { (wire(&gap.blocking_level)) } }
                            tr { th scope="row" { "Dedupe key" } td { code { (gap.dedupe_key) } } }
                            (text_row("Suggested owner", gap.suggested_owner.as_deref()))
                            tr {
                                th scope="row" { "Project" }
                                td {
                                    (project_cell(data.project.as_ref()))
                                    @if let Some(project) = &data.project {
                                        @if let Some(id) = &project.project_id {
                                            " " a.quiet href=(format!("/projects/{}", encode_component(id))) { "project page" }
                                        }
                                        @if project.key != project.label {
                                            div.quiet { code { (project.key) } }
                                        }
                                    }
                                }
                            }
                            tr { th scope="row" { "Source" } td { (provenance_cell(&data.provenance)) } }
                            tr { th scope="row" { "Created" } td { (age(parse_rfc3339(&gap.created_at), now)) } }
                            tr { th scope="row" { "Updated" } td { (age(parse_rfc3339(&gap.updated_at), now)) } }
                        }
                    }
                }
                section.panel aria-labelledby="gap-origin" {
                    h2 id="gap-origin" { "Origin" }
                    @if has_origin {
                        table.detail-table {
                            tbody {
                                tr {
                                    th scope="row" { "Task" }
                                    td {
                                        @match &gap.task_id {
                                            Some(task) => { a href=(format!("/bros/{}", encode_component(task))) { code { (task) } } }
                                            None => { span.quiet { "none" } }
                                        }
                                    }
                                }
                                tr {
                                    th scope="row" { "Thread" }
                                    td {
                                        @match &gap.thread_id {
                                            Some(thread) => { a href=(format!("/threads/{}", encode_component(thread))) { code { (thread) } } }
                                            None => { span.quiet { "none" } }
                                        }
                                    }
                                }
                                tr {
                                    th scope="row" { "Session" }
                                    td {
                                        @match &gap.session_id {
                                            Some(session) => { code { (session) } }
                                            None => { span.quiet { "none" } }
                                        }
                                    }
                                }
                                (text_row("Provider", gap.provider.as_deref()))
                                (text_row("Bro", gap.bro.as_deref()))
                            }
                        }
                    } @else {
                        (empty_state("No task, thread or session was recorded with this gap.", "bbox_gaps"))
                    }
                }
            }

            section.panel aria-labelledby="gap-need" {
                h2 id="gap-need" { "Need" }
                table.detail-table {
                    tbody {
                        (text_row("Wanted capability", Some(&gap.wanted_capability)))
                        (text_row("Missing primitive", gap.missing_primitive.as_deref()))
                        (text_row("Fallback used", gap.fallback_used.as_deref()))
                    }
                }
            }

            section.panel aria-labelledby="gap-evidence" {
                h2 id="gap-evidence" { "Evidence " span.quiet { "(" (gap.evidence.len()) ")" } }
                @if gap.evidence.is_empty() {
                    (empty_state("This gap records no evidence.", "bbox_gaps"))
                } @else {
                    ol.gap-evidence {
                        @for item in &gap.evidence {
                            li.prose { (linkified(item)) }
                        }
                    }
                }
            }

            @if let Some(notes) = gap.notes.as_deref().filter(|notes| !notes.trim().is_empty()) {
                section.panel aria-labelledby="gap-notes" {
                    h2 id="gap-notes" { "Notes" }
                    div.prose { (linkified(notes)) }
                }
            }

            section.panel aria-labelledby="gap-resolution" {
                h2 id="gap-resolution" { "Resolution" }
                table.detail-table {
                    tbody {
                        tr { th scope="row" { "State" } td { (resolution_label(gap.resolution)) } }
                        @if let Some(resolved) = &gap.resolved_at {
                            tr { th scope="row" { "Resolved" } td { (age(parse_rfc3339(resolved), now)) } }
                        }
                        (text_row("Note", gap.resolution_note.as_deref()))
                        tr { th scope="row" { "Issue" } td { (issue_cell(&issues)) } }
                    }
                }
            }

            section.panel aria-labelledby="gap-related" {
                h2 id="gap-related" { "Related gaps" }
                @if data.supersedes.is_none() && data.superseded_by.is_none() && data.siblings.is_empty() {
                    (empty_state("No gap supersedes this one or shares its dedupe key.", "bbox_gaps"))
                } @else {
                    table.detail-table {
                        tbody {
                            @if let Some(link) = &data.supersedes {
                                tr { th scope="row" { "Supersedes" } td { (gap_link_item(link)) } }
                            }
                            @if let Some(link) = &data.superseded_by {
                                tr { th scope="row" { "Superseded by" } td { (gap_link_item(link)) } }
                            }
                            @if !data.siblings.is_empty() {
                                tr {
                                    th scope="row" { "Same dedupe key" }
                                    td {
                                        ul.gap-siblings {
                                            @for link in &data.siblings {
                                                li { (gap_link_item(link)) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What a related-gaps panel on another page selects.
#[derive(Debug, Clone, Copy)]
pub(super) enum Related<'a> {
    /// Gaps filed from this thread, every resolution.
    Thread(&'a str),
    /// Gaps filed by this bro task, every resolution.
    Task(&'a str),
    /// Open gaps in this project, through the tool's project filter.
    Project(&'a str),
}

/// The related gaps and the total before truncation.
fn read_related(ui: &UiState, related: Related<'_>) -> anyhow::Result<(Vec<GapNote>, usize)> {
    let gaps = match related {
        Related::Project(project) => {
            let (view, params, _) = ui.server.resolve_gap_list(GapListParams {
                project: Some(project.to_string()),
                include_addressed: Some(false),
                ..GapListParams::default()
            })?;
            view.gaps.select(&params)?
        }
        Related::Thread(id) | Related::Task(id) => {
            let view = ui.server.session_gap_view(None)?;
            let mut gaps: Vec<GapNote> = view
                .gaps
                .all()
                .iter()
                .filter(|gap| match related {
                    Related::Thread(_) => gap.thread_id.as_deref() == Some(id),
                    _ => gap.task_id.as_deref() == Some(id),
                })
                .cloned()
                .collect();
            gaps.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            gaps
        }
    };
    let total = gaps.len();
    Ok((gaps.into_iter().take(RELATED_ROWS).collect(), total))
}

/// The gaps panel another page shows for `related`. Runs the reader inline:
/// call it from the blocking pool.
pub(super) fn related_panel(ui: &UiState, related: Related<'_>) -> Markup {
    let (empty, all_href) = match related {
        Related::Thread(_) => ("No gap was filed from this thread.", None),
        Related::Task(_) => ("No gap was filed by this task.", None),
        Related::Project(project) => (
            "No gaps are open in this project.",
            Some(format!("/gaps?project={}", encode_component(project))),
        ),
    };
    let now = Utc::now();
    html! {
        section.panel.gap-related aria-labelledby="related-gaps" {
            h2 id="related-gaps" { "Gaps" }
            @match read_related(ui, related) {
                Ok((gaps, total)) => {
                    @if gaps.is_empty() {
                        (empty_state(empty, "bbox_gaps"))
                    } @else {
                        table {
                            thead {
                                tr {
                                    th scope="col" { "Title" }
                                    th scope="col" { "Kind" }
                                    th scope="col" { "Impact" }
                                    th scope="col" { "Resolution" }
                                    th scope="col" { "Created" }
                                }
                            }
                            tbody {
                                @for gap in &gaps {
                                    tr {
                                        td {
                                            a href=(gap_href(&gap.id)) { (preview(&gap.title, 120)) }
                                            div.quiet { code { (gap.id) } }
                                        }
                                        td { (wire(&gap.gap_kind)) }
                                        td { (wire(&gap.impact)) }
                                        td { (resolution_label(gap.resolution)) }
                                        td { (age(parse_rfc3339(&gap.created_at), now)) }
                                    }
                                }
                            }
                        }
                        @if total > gaps.len() || all_href.is_some() {
                            p.pager {
                                @if total > gaps.len() {
                                    "Showing " (gaps.len()) " of " (total) ". "
                                }
                                @if let Some(href) = &all_href {
                                    a href=(href) { "All open gaps in this project" }
                                }
                            }
                        }
                    }
                    p.panel-source { "Source: " code { "bbox_gaps" } }
                }
                Err(error) => { (error_fragment("bbox_gaps", &format!("{error:#}"))) }
            }
        }
    }
}

/// Open gaps per impact level, most severe first, for the overview.
pub(super) fn open_impacts(gaps: &[GapNote]) -> Vec<(&'static str, usize)> {
    IMPACT_ORDER
        .iter()
        .map(|impact| {
            let count = gaps
                .iter()
                .filter(|gap| gap.impact == *impact && gap.resolution != GapResolution::Addressed)
                .count();
            (wire(impact), count)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;

    use super::super::readouts::test_support::get;
    use super::*;
    use crate::server::SharedState;

    fn ingest(shared: &SharedState, record: serde_json::Value) {
        let gap: GapNote = serde_json::from_value(record).unwrap();
        shared.gaps.write().ingest(gap).unwrap();
    }

    /// Four gaps: a critical unresolved one from a task and thread, a
    /// medium acknowledged one tracked by an issue, an addressed one that
    /// supersedes nothing, and a low unresolved refiling under the
    /// addressed one's dedupe key.
    fn seed(shared: &SharedState) {
        ingest(
            shared,
            serde_json::json!({
                "id": "gap-0000000a", "title": "Roster loses <tasks> on reconnect",
                "gap_kind": "workflow", "domain": "bro-orchestration",
                "wanted_capability": "Keep the roster across reconnects; see https://example.test/doc.",
                "evidence": ["seen in https://example.test/log", "second item"],
                "impact": "critical", "blocking_level": "blocks_task",
                "dedupe_key": "workflow/bro-orchestration/roster-reconnect",
                "task_id": "task-alpha", "thread_id": "thread-0000beef",
                "session_id": "sess-1", "provider": "codex",
                "created_at": "2026-09-03T00:00:00Z"
            }),
        );
        ingest(
            shared,
            serde_json::json!({
                "id": "gap-0000000b", "title": "Render skips a projection",
                "gap_kind": "tooling", "domain": "render-lifecycle",
                "wanted_capability": "Render every projection.",
                "impact": "medium", "resolution": "acknowledged",
                "resolution_note": "issue: https://github.com/acme/widgets/issues/136.",
                "dedupe_key": "tooling/render-lifecycle/projection",
                "created_at": "2026-09-02T00:00:00Z"
            }),
        );
        ingest(
            shared,
            serde_json::json!({
                "id": "gap-0000000c", "title": "Old resolver keys threads by path",
                "gap_kind": "tooling", "domain": "project-resolver",
                "wanted_capability": "Key threads by project id.",
                "impact": "high", "resolution": "addressed",
                "resolution_note": "Refiled as gap-0000000d.",
                "superseded_by": "gap-0000000d",
                "dedupe_key": "tooling/project-resolver/thread-keys",
                "created_at": "2026-09-01T00:00:00Z"
            }),
        );
        ingest(
            shared,
            serde_json::json!({
                "id": "gap-0000000d", "title": "Resolver keys threads by path",
                "gap_kind": "tooling", "domain": "project-resolver",
                "wanted_capability": "Key threads by project id.",
                "impact": "low", "supersedes": "gap-0000000c",
                "dedupe_key": "tooling/project-resolver/thread-keys",
                "created_at": "2026-09-04T00:00:00Z"
            }),
        );
    }

    fn seeded() -> (tempfile::TempDir, Arc<SharedState>) {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        seed(&shared);
        (dir, shared)
    }

    #[test]
    fn segments_split_urls_and_keep_trailing_punctuation_as_text() {
        assert_eq!(
            segments("see https://a.test/x. and (http://b.test/y) javascript:alert(1)"),
            vec![
                Segment::Text("see "),
                Segment::Url("https://a.test/x"),
                Segment::Text(". and ("),
                Segment::Url("http://b.test/y"),
                Segment::Text(") javascript:alert(1)"),
            ]
        );
        assert_eq!(
            segments("bare https:// only"),
            vec![Segment::Text("bare https://"), Segment::Text(" only"),]
        );
        let markup = linkified("<b> https://a.test/?q=<x>").into_string();
        assert!(markup.contains("&lt;b&gt;"), "{markup}");
        assert!(markup.contains("href=\"https://a.test/?q=\""), "{markup}");
    }

    #[test]
    fn issue_links_name_github_issues_and_pull_requests_once_each() {
        let links = issue_links(
            "issue: https://github.com/acme/widgets/issues/136, PR https://github.com/acme/widgets/pull/140 \
             again https://github.com/acme/widgets/issues/136 and https://github.com/acme/widgets/tree/main",
        );
        assert_eq!(
            links,
            vec![
                IssueLink {
                    label: "widgets#136".into(),
                    url: "https://github.com/acme/widgets/issues/136".into(),
                },
                IssueLink {
                    label: "PR widgets#140".into(),
                    url: "https://github.com/acme/widgets/pull/140".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn empty_store_lists_nothing_and_names_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (status, body) = get(shared, "/gaps").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("<h1>Gaps</h1>"), "{body}");
        assert!(
            body.contains("href=\"/gaps\" aria-current=\"page\""),
            "{body}"
        );
        assert!(body.contains("No gaps are stored."), "{body}");
        assert!(body.contains("bbox_gaps"));
        assert!(!body.contains("error-fragment"), "{body}");
    }

    #[tokio::test]
    async fn default_list_shows_open_gaps_newest_first_with_impacts_and_issue_links() {
        let (_dir, shared) = seeded();
        let (status, body) = get(shared, "/gaps").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.contains("gap-0000000c"),
            "addressed gaps are hidden by default"
        );
        let d = body.find("gap-0000000d").expect("newest open gap listed");
        let a = body.find("gap-0000000a").expect("critical gap listed");
        let b = body.find("gap-0000000b").expect("acknowledged gap listed");
        assert!(d < a && a < b, "rows are newest first");
        assert!(
            body.contains("Roster loses &lt;tasks&gt; on reconnect"),
            "titles escape"
        );
        assert!(body.contains("3 matching: "), "{body}");
        assert!(body.contains("href=\"/gaps?impact=critical\""), "{body}");
        assert!(
            body.contains(
                "<a href=\"https://github.com/acme/widgets/issues/136\" rel=\"noreferrer\" \
             title=\"https://github.com/acme/widgets/issues/136\">widgets#136</a>"
            ),
            "{body}"
        );
        assert!(body.contains("hx-get=\"/fragments/gaps/table\""), "{body}");
        assert!(body.contains("hx-trigger=\"every 30s\""), "{body}");
    }

    #[tokio::test]
    async fn filters_select_by_resolution_kind_impact_and_text() {
        let (_dir, shared) = seeded();
        let (_, body) = get(shared.clone(), "/gaps?resolution=all").await;
        assert!(body.contains("gap-0000000c"), "all includes addressed");
        let (_, body) = get(shared.clone(), "/gaps?resolution=addressed").await;
        assert!(body.contains("gap-0000000c") && !body.contains("gap-0000000a"));
        let (_, body) = get(shared.clone(), "/gaps?kind=workflow").await;
        assert!(body.contains("gap-0000000a") && !body.contains("gap-0000000b"));
        let (_, body) = get(shared.clone(), "/gaps?impact=medium&kind=").await;
        assert!(body.contains("gap-0000000b") && !body.contains("gap-0000000a"));
        let (_, body) = get(shared.clone(), "/gaps?q=projection").await;
        assert!(body.contains("gap-0000000b") && !body.contains("gap-0000000d"));
        let (_, body) = get(shared.clone(), "/gaps?domain=nothing-here").await;
        assert!(body.contains("No gaps match these filters."), "{body}");
        let (_, body) = get(shared, "/gaps?impact=enormous").await;
        assert!(
            body.contains("error-fragment"),
            "an invalid filter is an error, not a broader list"
        );
    }

    #[tokio::test]
    async fn table_fragment_renders_without_the_shell() {
        let (_dir, shared) = seeded();
        let (status, body) = get(shared, "/fragments/gaps/table?kind=workflow").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("gap-0000000a"));
        assert!(!body.contains("<nav"), "{body}");
    }

    #[tokio::test]
    async fn detail_shows_the_record_origin_links_and_resolution() {
        let (_dir, shared) = seeded();
        let (status, body) = get(shared.clone(), "/gaps/gap-0000000a").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("<h1>Roster loses &lt;tasks&gt; on reconnect</h1>"),
            "{body}"
        );
        assert!(body.contains("href=\"/bros/task-alpha\""), "{body}");
        assert!(body.contains("href=\"/threads/thread-0000beef\""), "{body}");
        assert!(
            body.contains("<a href=\"https://example.test/doc\" rel=\"noreferrer\">"),
            "{body}"
        );
        assert!(
            body.contains("<a href=\"https://example.test/log\" rel=\"noreferrer\">"),
            "{body}"
        );
        assert!(body.contains("workflow/bro-orchestration/roster-reconnect"));
        assert!(body.contains("blocks_task"));

        let (_, body) = get(shared.clone(), "/gaps/0000000b").await;
        assert!(body.contains("No task, thread or session was recorded with this gap."));
        assert!(body.contains("widgets#136"), "{body}");
    }

    #[tokio::test]
    async fn detail_links_supersession_and_same_dedupe_key_both_ways() {
        let (_dir, shared) = seeded();
        let (_, body) = get(shared.clone(), "/gaps/gap-0000000d").await;
        assert!(body.contains("Supersedes"), "{body}");
        assert!(body.contains("href=\"/gaps/gap-0000000c\""), "{body}");
        assert!(body.contains("Old resolver keys threads by path"));
        let (status, body) = get(shared, "/gaps/gap-0000000c").await;
        assert_eq!(status, StatusCode::OK, "an addressed gap still has a page");
        assert!(body.contains("Superseded by"), "{body}");
        assert!(body.contains("href=\"/gaps/gap-0000000d\""), "{body}");
        assert!(body.contains("Same dedupe key"), "{body}");
    }

    #[tokio::test]
    async fn an_unknown_gap_is_a_404_page_and_fragment() {
        let (_dir, shared) = seeded();
        let (status, body) = get(shared.clone(), "/gaps/gap-ffffffff").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("No gap has the id gap-ffffffff."), "{body}");
        let (status, body) = get(shared, "/fragments/gaps/not-a-gap").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.contains("<nav"));
    }

    #[tokio::test]
    async fn overview_links_open_gaps_by_impact() {
        let (_dir, shared) = seeded();
        let (_, body) = get(shared, "/").await;
        assert!(body.contains("<a href=\"/gaps\">Open gaps</a>"), "{body}");
        assert!(
            body.contains("<a href=\"/gaps?impact=critical\">critical</a>"),
            "{body}"
        );
        assert!(
            body.contains("<span class=\"quiet\">high</span>"),
            "addressed gaps do not count: {body}"
        );
    }

    #[tokio::test]
    async fn project_panel_lists_only_that_projects_open_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        for (id, project, resolution) in [
            ("gap-000000e1", "/work/alpha", "unresolved"),
            ("gap-000000e2", "/work/alpha", "addressed"),
            ("gap-000000e3", "/work/beta", "unresolved"),
        ] {
            ingest(
                &shared,
                serde_json::json!({
                    "id": id, "title": format!("title {id}"), "gap_kind": "tooling",
                    "domain": "synthetic", "wanted_capability": "w",
                    "dedupe_key": format!("tooling/synthetic/{id}"),
                    "project": project, "resolution": resolution,
                    "created_at": "2026-09-01T00:00:00Z"
                }),
            );
        }
        let ui = UiState::new(shared);
        let panel = related_panel(&ui, Related::Project("/work/alpha")).into_string();
        assert!(panel.contains("gap-000000e1"), "{panel}");
        assert!(
            !panel.contains("gap-000000e2"),
            "addressed gaps are left out"
        );
        assert!(
            !panel.contains("gap-000000e3"),
            "other projects are left out"
        );
        assert!(
            panel.contains("href=\"/gaps?project=%2Fwork%2Falpha\""),
            "{panel}"
        );
    }

    #[tokio::test]
    async fn related_panels_select_by_thread_and_task() {
        let (_dir, shared) = seeded();
        let ui = UiState::new(shared);
        let thread = related_panel(&ui, Related::Thread("thread-0000beef")).into_string();
        assert!(thread.contains("href=\"/gaps/gap-0000000a\""), "{thread}");
        assert!(!thread.contains("gap-0000000b"));
        let task = related_panel(&ui, Related::Task("task-alpha")).into_string();
        assert!(task.contains("gap-0000000a"), "{task}");
        let none = related_panel(&ui, Related::Task("task-other")).into_string();
        assert!(none.contains("No gap was filed by this task."), "{none}");
    }
}
