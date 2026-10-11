//! `/threads` and `/threads/{id}`: the thread list with status and project
//! filters, and one thread's notes, participants and linked entities.
//!
//! The list reads through the matcher that `bbox_thread_list` uses; the
//! detail reads the same stored record that `bbox_thread` pages. Both run
//! on the blocking pool: the project resolver may probe the filesystem and
//! the store lock can wait behind a write.
//!
//! Threads are keyed to a project id when the resolver knew the project at
//! the time they were opened, else to the raw selector the caller passed.
//! The project filter offers one choice per project, not per stored key:
//! every stored key is resolved through the resolver the tool's filter lane
//! uses, keys that name the same project fold into one choice labelled with
//! the project's name, and a selector no project claims stays a choice of
//! its own under the selector itself. Selecting a choice lists the threads
//! stored under any of its keys; a project value the choices do not cover
//! falls through to the tool's own filter semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use bbox_corpus_core::project_selector::ProjectSelectorRequest;
use bbox_threads::threads::{EdgeTarget, Thread, ThreadListParams, ThreadOrigin, ThreadStatus};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::display::{age, encode_component, parse_rfc3339, preview};
use super::projects::project_names;

/// Rows per list page.
const PAGE_SIZE: usize = 100;

/// Status filter choices, in display order, as `(value, label)`. `live`
/// (the default) lists open and active threads; `all` lists every status;
/// the rest select one.
const STATUS_CHOICES: [(&str, &str); 6] = [
    ("live", "open and active"),
    ("all", "all statuses"),
    ("open", "open"),
    ("active", "active"),
    ("resolved", "resolved"),
    ("promoted", "promoted"),
];

/// What the status filter selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusSelection<'a> {
    /// Open and active threads: the default, and what an unknown value means.
    Live,
    /// Every status, resolved and promoted included.
    All,
    One(&'a str),
}

/// The list's query string: plain GET parameters, so filtering works
/// without JavaScript.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
}

impl ListQuery {
    fn selection(&self) -> StatusSelection<'_> {
        match self.status.as_deref().map(str::trim) {
            None | Some("") | Some("live") => StatusSelection::Live,
            Some("all") => StatusSelection::All,
            Some(status) => StatusSelection::One(status),
        }
    }

    /// The query value that names the selection, absent for the default.
    fn status(&self) -> Option<&str> {
        match self.selection() {
            StatusSelection::Live => None,
            StatusSelection::All => Some("all"),
            StatusSelection::One(status) => Some(status),
        }
    }

    fn project(&self) -> Option<&str> {
        self.project
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
    }

    fn offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }

    /// `/threads` with these filters at `offset`.
    fn href(&self, offset: usize) -> String {
        let mut params = Vec::new();
        if let Some(status) = self.status() {
            params.push(format!("status={}", encode_component(status)));
        }
        if let Some(project) = self.project() {
            params.push(format!("project={}", encode_component(project)));
        }
        if offset > 0 {
            params.push(format!("offset={offset}"));
        }
        if params.is_empty() {
            "/threads".to_string()
        } else {
            format!("/threads?{}", params.join("&"))
        }
    }
}

/// The state class a thread status renders under.
fn status_state(status: &ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::Open => "waiting",
        ThreadStatus::Active => "running",
        ThreadStatus::Resolved | ThreadStatus::Promoted => "finished",
    }
}

/// The stored wire name of a thread enum (`open`, `relates_to`, ...).
fn wire<T: AsRef<str>>(value: &T) -> &str {
    value.as_ref()
}

fn status_label(status: &ThreadStatus) -> Markup {
    html! {
        span class=(format!("state state-{}", status_state(status))) { (wire(status)) }
    }
}

/// The key a thread is stored under, as `bbox_thread_list` reports it: the
/// resolver's project id when stamped, else the stored selector.
fn thread_project(thread: &Thread) -> Option<&str> {
    thread
        .project_id
        .as_deref()
        .or_else(|| (!thread.project.is_empty()).then_some(thread.project.as_str()))
}

/// One choice on the project filter: the stored keys that name one project,
/// or one selector no project claims.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectChoice {
    /// The query value: the project id, else the selector itself.
    value: String,
    /// The option text: the project's name, else its id, else the selector.
    label: String,
    /// The project behind the choice; absent for an unclaimed selector.
    project_id: Option<String>,
    /// Every stored key that selects this choice.
    keys: BTreeSet<String>,
}

impl ProjectChoice {
    /// How a row names its project: the choice's label and value, with the
    /// stored key kept for the hover text.
    fn reference(&self, key: &str) -> ProjectRef {
        ProjectRef {
            value: self.value.clone(),
            label: self.label.clone(),
            project_id: self.project_id.clone(),
            key: key.to_string(),
        }
    }
}

/// A thread's project as a page names it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectRef {
    value: String,
    label: String,
    project_id: Option<String>,
    key: String,
}

/// The project filter choices for `keys`, plus the resolution of every key
/// and of `extra` (a query value that may not be a stored key): keys the
/// resolver maps to one project fold into that project's choice, the rest
/// stand alone. Resolution runs over one catalog snapshot; when the
/// resolver is unavailable every key stands alone.
fn project_choices(
    ui: &UiState,
    keys: &BTreeSet<String>,
    extra: Option<&str>,
) -> (Vec<ProjectChoice>, BTreeMap<String, Option<String>>) {
    let wanted: BTreeSet<&str> = keys.iter().map(String::as_str).chain(extra).collect();
    let resolved: BTreeMap<String, Option<String>> = ui
        .server
        .with_project_resolver(|engine| {
            wanted
                .iter()
                .map(|raw| {
                    let id = engine
                        .resolve(&ProjectSelectorRequest::filter(*raw))
                        .ok()
                        .and_then(|resolution| resolution.project_id().map(str::to_owned));
                    (raw.to_string(), id)
                })
                .collect()
        })
        .unwrap_or_else(|_| wanted.iter().map(|raw| (raw.to_string(), None)).collect());

    let mut by_project: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let mut literals: Vec<&str> = Vec::new();
    for key in keys {
        match resolved.get(key).and_then(Option::as_deref) {
            Some(id) => {
                by_project.entry(id).or_default().insert(key.clone());
            }
            None => literals.push(key),
        }
    }
    let names = project_names(ui);
    let mut choices: Vec<ProjectChoice> = by_project
        .into_iter()
        .map(|(id, keys)| ProjectChoice {
            value: id.to_string(),
            label: names.get(id).cloned().unwrap_or_else(|| id.to_string()),
            project_id: Some(id.to_string()),
            keys,
        })
        .collect();
    choices.sort_by(|a, b| {
        a.label
            .to_lowercase()
            .cmp(&b.label.to_lowercase())
            .then_with(|| a.value.cmp(&b.value))
    });
    choices.extend(literals.into_iter().map(|key| ProjectChoice {
        value: key.to_string(),
        label: key.to_string(),
        project_id: None,
        keys: BTreeSet::from([key.to_string()]),
    }));
    (choices, resolved)
}

/// The choice a filter value selects: by value, then by the project the
/// value resolves to, then by stored key.
fn select_choice<'c>(
    choices: &'c [ProjectChoice],
    resolved: &BTreeMap<String, Option<String>>,
    wanted: &str,
) -> Option<&'c ProjectChoice> {
    choices
        .iter()
        .find(|choice| choice.value == wanted)
        .or_else(|| {
            let id = resolved.get(wanted)?.as_deref()?;
            choices
                .iter()
                .find(|choice| choice.project_id.as_deref() == Some(id))
        })
        .or_else(|| choices.iter().find(|choice| choice.keys.contains(wanted)))
}

fn thread_title(thread: &Thread) -> String {
    match thread
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        Some(name) => preview(name, 120),
        None => preview(&thread.topic, 120),
    }
}

struct ThreadRow {
    id: String,
    title: String,
    workflow: bool,
    status: ThreadStatus,
    project: Option<ProjectRef>,
    notes: usize,
    updated: Option<DateTime<Utc>>,
}

struct ListData {
    rows: Vec<ThreadRow>,
    total: usize,
    /// The filter choices: one per project a stored thread names, then one
    /// per selector no project claims.
    projects: Vec<ProjectChoice>,
    /// The value of the choice the query selected, when it named one.
    selected: Option<String>,
    /// Whether the store holds any thread at all.
    store_empty: bool,
}

/// `bbox_thread_list` reader: every status and origin, filtered by the
/// query, newest activity first.
fn read_list(ui: &UiState, query: &ListQuery) -> anyhow::Result<ListData> {
    let (keys, store_empty) = {
        let store = ui.shared.threads.read();
        let all = store.all();
        let keys: BTreeSet<String> = all
            .iter()
            .filter_map(|thread| thread_project(thread).map(str::to_string))
            .collect();
        (keys, all.is_empty())
    };
    let (choices, resolved) = project_choices(ui, &keys, query.project());
    let selected = query
        .project()
        .and_then(|wanted| select_choice(&choices, &resolved, wanted));
    let choice_of: BTreeMap<&str, &ProjectChoice> = choices
        .iter()
        .flat_map(|choice| choice.keys.iter().map(move |key| (key.as_str(), choice)))
        .collect();

    // The store's default already hides resolved and promoted threads; the
    // page opts into them with `all` or by naming one of them.
    let (status, include_resolved) = match query.selection() {
        StatusSelection::Live => (None, false),
        StatusSelection::All => (None, true),
        StatusSelection::One(status) => (Some(status.to_string()), true),
    };
    let mut filters = ThreadListParams {
        status,
        include_resolved: Some(include_resolved),
        include_workflows: Some(true),
        ..ThreadListParams::default()
    };
    // A value the choices cover selects its stored keys here; any other
    // value keeps the tool's own filter semantics.
    if let (Some(project), None) = (query.project(), selected) {
        filters.project = Some(project.to_string());
    }
    let filters = ui.server.thread_list_filters(filters);

    let store = ui.shared.threads.read();
    let matching: Vec<&Thread> = store
        .matching_threads(&filters)?
        .into_iter()
        .filter(|thread| match selected {
            Some(choice) => thread_project(thread).is_some_and(|key| choice.keys.contains(key)),
            None => true,
        })
        .collect();
    let total = matching.len();
    let rows = matching
        .into_iter()
        .skip(query.offset())
        .take(PAGE_SIZE)
        .map(|thread| ThreadRow {
            id: thread.id.clone(),
            title: thread_title(thread),
            workflow: thread.origin == Some(ThreadOrigin::Workflow),
            status: thread.status.clone(),
            project: thread_project(thread)
                .and_then(|key| Some(choice_of.get(key)?.reference(key))),
            notes: thread.notes.len(),
            updated: parse_rfc3339(&thread.last_activity),
        })
        .collect();
    Ok(ListData {
        rows,
        total,
        selected: selected.map(|choice| choice.value.clone()),
        projects: choices,
        store_empty,
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

/// `GET /threads`
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
        div id="threads-table" {
            (list_table(&query, &data, Utc::now()))
        }
    };
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(&principal),
            Some(Section::Threads),
            "Threads",
            content,
        ),
    )
}

/// `GET /fragments/threads/table`: the list table the page renders.
pub(crate) async fn table_fragment(
    State(ui): State<Arc<UiState>>,
    Query(query): Query<ListQuery>,
) -> Response {
    let (query, data) = list_data(&ui, query).await;
    html_response(StatusCode::OK, list_table(&query, &data, Utc::now()))
}

fn filter_form(query: &ListQuery, data: &ListData) -> Markup {
    let status = query.status().unwrap_or("live");
    let project = query.project();
    // The choice the query selected, so a value given as a path or an
    // alias still shows its project selected.
    let current = data.selected.as_deref();
    html! {
        form.filters method="get" action="/threads" {
            label {
                "Status "
                select name="status" {
                    @for (choice, label) in STATUS_CHOICES {
                        option value=(choice) selected[choice == status] { (label) }
                    }
                }
            }
            label {
                "Project "
                select name="project" {
                    option value="" selected[project.is_none()] { "all projects" }
                    @for choice in &data.projects {
                        option value=(choice.value) selected[Some(choice.value.as_str()) == current] {
                            (preview(&choice.label, 80))
                        }
                    }
                }
            }
            button type="submit" { "Filter" }
            @if query.status().is_some() || project.is_some() {
                a href="/threads" { "Clear filters" }
            }
        }
    }
}

fn list_table(query: &ListQuery, data: &Result<ListData, String>, now: DateTime<Utc>) -> Markup {
    let data = match data {
        Ok(data) => data,
        Err(error) => return error_fragment("bbox_thread_list", error),
    };
    let offset = query.offset();
    html! {
        @if data.rows.is_empty() {
            @if data.store_empty {
                (empty_state("No threads are stored.", "bbox_thread_list"))
            } @else if data.total > 0 {
                (empty_state("This page is past the last matching thread.", "bbox_thread_list"))
            } @else {
                (empty_state("No threads match these filters.", "bbox_thread_list"))
            }
        } @else {
            table.threads-table {
                thead {
                    tr {
                        th scope="col" { "Title" }
                        th scope="col" { "Status" }
                        th scope="col" { "Project" }
                        th.num scope="col" { "Notes" }
                        th scope="col" { "Updated" }
                    }
                }
                tbody {
                    @for row in &data.rows {
                        tr {
                            td {
                                a href=(format!("/threads/{}", encode_component(&row.id))) { (row.title) }
                                @if row.workflow { " " span.badge { "workflow" } }
                                div.quiet { code { (row.id) } }
                            }
                            td { (status_label(&row.status)) }
                            td { (project_cell(row.project.as_ref())) }
                            td.num { (row.notes) }
                            td { (age(row.updated, now)) }
                        }
                    }
                }
            }
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
        p.panel-source { "Source: " code { "bbox_thread_list" } }
    }
}

/// A thread's project: the name linking to the threads it filters, with the
/// stored key on hover.
fn project_cell(project: Option<&ProjectRef>) -> Markup {
    html! {
        @match project {
            Some(project) => {
                a href=(format!("/threads?project={}", encode_component(&project.value)))
                    title=(project.key) {
                    (preview(&project.label, 60))
                }
            }
            None => { span.quiet { "none" } }
        }
    }
}

/// `bbox_thread` reader: the stored record for this id, with its project
/// named the way the list names it.
async fn read_thread(
    ui: &Arc<UiState>,
    id: String,
) -> Result<Option<(Thread, Option<ProjectRef>)>, String> {
    let reader_ui = ui.clone();
    tokio::task::spawn_blocking(move || {
        let thread = reader_ui.shared.threads.read().thread_by_id(&id).cloned()?;
        let project = thread_project(&thread).map(|key| {
            let (choices, _) =
                project_choices(&reader_ui, &BTreeSet::from([key.to_string()]), None);
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
        });
        Some((thread, project))
    })
    .await
    .map_err(|error| error.to_string())
}

/// `GET /threads/{id}`
pub(crate) async fn detail_page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let (status, title, content) = match read_thread(&ui, id.clone()).await {
        Ok(Some((thread, project))) => (
            StatusCode::OK,
            thread_title(&thread),
            thread_detail(&thread, project.as_ref(), Utc::now()),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            "Thread not found".to_string(),
            missing_thread(&id),
        ),
        Err(error) => (
            StatusCode::OK,
            "Thread".to_string(),
            error_fragment("bbox_thread", &error),
        ),
    };
    html_response(
        status,
        layout(
            &ui.shell(&principal),
            Some(Section::Threads),
            &title,
            content,
        ),
    )
}

/// `GET /fragments/threads/{id}`: the detail pane the page renders.
pub(crate) async fn detail_fragment(
    State(ui): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> Response {
    match read_thread(&ui, id.clone()).await {
        Ok(Some((thread, project))) => html_response(
            StatusCode::OK,
            thread_detail(&thread, project.as_ref(), Utc::now()),
        ),
        Ok(None) => html_response(StatusCode::NOT_FOUND, missing_thread(&id)),
        Err(error) => html_response(StatusCode::OK, error_fragment("bbox_thread", &error)),
    }
}

fn missing_thread(id: &str) -> Markup {
    html! {
        (empty_state(&format!("No thread has the id {id}."), "bbox_thread"))
        p { a href="/threads" { "All threads" } }
    }
}

fn thread_detail(thread: &Thread, project: Option<&ProjectRef>, now: DateTime<Utc>) -> Markup {
    html! {
        div.thread-detail {
            section.panel aria-labelledby="thread-summary" {
                h2 id="thread-summary" { "Summary" }
                table.detail-table {
                    tbody {
                        tr { th scope="row" { "Id" } td { code { (thread.id) } } }
                        @if let Some(name) = &thread.name {
                            tr { th scope="row" { "Name" } td { (name) } }
                        }
                        tr { th scope="row" { "Topic" } td.prose { (thread.topic) } }
                        tr { th scope="row" { "Status" } td { (status_label(&thread.status)) } }
                        @if let Some(kind) = thread.kind {
                            tr { th scope="row" { "Kind" } td { (wire(&kind)) } }
                        }
                        @if let Some(origin) = thread.origin {
                            tr { th scope="row" { "Origin" } td { (wire(&origin)) } }
                        }
                        tr {
                            th scope="row" { "Project" }
                            td {
                                (project_cell(project))
                                @if let Some(project) = project {
                                    @if let Some(id) = &project.project_id {
                                        " " a.quiet href=(format!("/projects/{}", encode_component(id))) { "project page" }
                                    }
                                    @if project.key != project.label {
                                        div.quiet { code { (project.key) } }
                                    }
                                }
                            }
                        }
                        tr { th scope="row" { "Created" } td { (age(parse_rfc3339(&thread.created_at), now)) } }
                        tr { th scope="row" { "Last activity" } td { (age(parse_rfc3339(&thread.last_activity), now)) } }
                        @if let Some(resolved) = &thread.resolved_at {
                            tr { th scope="row" { "Resolved" } td { (age(parse_rfc3339(resolved), now)) } }
                        }
                        @if let Some(promoted) = &thread.promoted_to {
                            tr { th scope="row" { "Promoted to" } td { code { (promoted) } } }
                        }
                    }
                }
            }

            section.panel aria-labelledby="thread-notes" {
                h2 id="thread-notes" { "Notes " span.quiet { "(" (thread.notes.len()) ")" } }
                @if thread.notes.is_empty() {
                    (empty_state("This thread has no notes.", "bbox_thread"))
                } @else {
                    ol.thread-notes {
                        @for (index, note) in thread.notes.iter().enumerate() {
                            li id=(format!("note-{}", index + 1)) {
                                div.note-meta { "Note " (index + 1) }
                                div.note-body.prose { (note) }
                            }
                        }
                    }
                    p.panel-source {
                        "Notes are stored in append order without an author or a time; "
                        "participants and linked entities carry their own times. Source: "
                        code { "bbox_thread" }
                    }
                }
            }

            section.panel aria-labelledby="thread-participants" {
                h2 id="thread-participants" { "Participants" }
                @if thread.sessions.is_empty() {
                    (empty_state("No sessions are linked to this thread.", "bbox_thread"))
                } @else {
                    table {
                        thead {
                            tr {
                                th scope="col" { "Name" }
                                th scope="col" { "Session" }
                                th scope="col" { "Provider" }
                                th scope="col" { "Linked" }
                            }
                        }
                        tbody {
                            @for session in &thread.sessions {
                                tr {
                                    td {
                                        @match &session.name {
                                            Some(name) => { (name) }
                                            None => { span.quiet { "unnamed" } }
                                        }
                                    }
                                    td { code { (session.session_id) } }
                                    td { (session.provider) }
                                    td { (age(parse_rfc3339(&session.linked_at), now)) }
                                }
                            }
                        }
                    }
                }
            }

            section.panel aria-labelledby="thread-links" {
                h2 id="thread-links" { "Linked entities" }
                @if thread.edges.is_empty() {
                    (empty_state("This thread links to nothing.", "bbox_thread"))
                } @else {
                    table {
                        thead {
                            tr {
                                th scope="col" { "Relation" }
                                th scope="col" { "Target" }
                                th scope="col" { "Kind" }
                                th scope="col" { "Note" }
                                th scope="col" { "Linked" }
                            }
                        }
                        tbody {
                            @for edge in &thread.edges {
                                tr {
                                    td { (wire(&edge.kind)) }
                                    td {
                                        @if edge.target_type == EdgeTarget::Thread {
                                            a href=(format!("/threads/{}", encode_component(&edge.target))) {
                                                code { (edge.target) }
                                            }
                                        } @else {
                                            code { (edge.target) }
                                        }
                                    }
                                    td { (wire(&edge.target_type)) }
                                    td.prose {
                                        @match &edge.note {
                                            Some(note) => { (note) }
                                            None => { span.quiet { "none" } }
                                        }
                                    }
                                    td { (age(parse_rfc3339(&edge.created_at), now)) }
                                }
                            }
                        }
                    }
                }
            }

            @if let Some(doc) = thread.handoff_doc.as_deref().filter(|doc| !doc.trim().is_empty()) {
                section.panel aria-labelledby="thread-handoff" {
                    h2 id="thread-handoff" { "Handoff" }
                    pre.thread-handoff { (doc) }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{UiState, build_ui_router};
    use crate::server::SharedState;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn get(shared: Arc<SharedState>, uri: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .uri(uri)
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                53100,
            )))
            .header("x-forwarded-email", "operator@example.test")
            .body(Body::empty())
            .unwrap();
        let response = build_ui_router(UiState::new(shared))
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn mutate(shared: &SharedState, params: serde_json::Value) -> String {
        let params: crate::threads::ThreadParams = serde_json::from_value(params).unwrap();
        shared.threads.write().thread(&params).unwrap()
    }

    /// Two threads: an active one under `/work/alpha` with notes, a linked
    /// session and an edge to the second, and an open one under
    /// `/work/beta`. Returns their ids.
    fn seed(shared: &SharedState) -> (String, String) {
        mutate(
            shared,
            serde_json::json!({
                "action": "open",
                "topic": "Audit the roster projection",
                "project": "/work/alpha",
                "note": "first note body",
                "session_id": "sess-planner-1",
                "provider": "codex",
                "session_name": "planner",
            }),
        );
        mutate(
            shared,
            serde_json::json!({
                "action": "open",
                "topic": "Draft the <release> notes",
                "project": "/work/beta",
            }),
        );
        let (alpha, beta) = {
            let store = shared.threads.read();
            let find = |topic: &str| {
                store
                    .all()
                    .iter()
                    .find(|thread| thread.topic.starts_with(topic))
                    .unwrap()
                    .id
                    .clone()
            };
            (find("Audit"), find("Draft"))
        };
        mutate(
            shared,
            serde_json::json!({"action": "continue", "id": alpha, "note": "second note body"}),
        );
        mutate(
            shared,
            serde_json::json!({
                "action": "link", "id": alpha, "target": beta, "edge": "relates_to",
                "note": "release depends on the audit",
            }),
        );
        (alpha, beta)
    }

    #[tokio::test]
    async fn thread_list_renders_every_seeded_thread() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (alpha, beta) = seed(&shared);

        let (status, body) = get(shared.clone(), "/threads").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("href=\"/threads\" aria-current=\"page\""));
        assert!(body.contains("<h1>Threads</h1>"));
        assert!(
            body.contains(&format!("href=\"/threads/{alpha}\"")),
            "{body}"
        );
        assert!(body.contains(&format!("href=\"/threads/{beta}\"")));
        assert!(
            body.contains("Draft the &lt;release&gt; notes"),
            "topics are escaped"
        );
        assert!(body.contains("<span class=\"state state-running\">active</span>"));
        assert!(body.contains("<span class=\"state state-waiting\">open</span>"));
        assert!(
            body.contains("<td class=\"num\">2</td>"),
            "alpha has two notes"
        );
        assert!(body.contains("/work/alpha") && body.contains("/work/beta"));
        assert!(body.contains("class=\"filters\"") && body.contains("action=\"/threads\""));
        assert!(
            body.contains(" UTC\""),
            "ages carry the absolute time on hover"
        );
    }

    #[tokio::test]
    async fn thread_list_filters_by_status_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (alpha, beta) = seed(&shared);

        let (_, open) = get(shared.clone(), "/threads?status=open").await;
        assert!(open.contains(&beta) && !open.contains(&format!("/threads/{alpha}")));
        assert!(
            open.contains("<option value=\"open\" selected>open</option>"),
            "{open}"
        );

        let (_, alpha_only) = get(shared.clone(), "/threads?project=%2Fwork%2Falpha").await;
        assert!(alpha_only.contains(&format!("/threads/{alpha}")));
        assert!(!alpha_only.contains(&format!("href=\"/threads/{beta}\"")));
        assert!(alpha_only.contains("Clear filters"));

        let (_, none) = get(shared.clone(), "/threads?status=resolved").await;
        assert!(none.contains("No threads match these filters."), "{none}");

        let (status, fragment) = get(shared.clone(), "/fragments/threads/table?status=open").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!fragment.contains("<html"), "a fragment renders no shell");
        assert!(fragment.contains(&beta) && !fragment.contains(&format!("/threads/{alpha}")));

        // A resolved thread is opt-in: hidden by default, listed under `all`
        // and under its own status.
        mutate(
            &shared,
            serde_json::json!({"action": "resolve", "id": beta, "note": "shipped"}),
        );
        let (_, live) = get(shared.clone(), "/threads").await;
        assert!(
            live.contains(&format!("href=\"/threads/{alpha}\"")),
            "{live}"
        );
        assert!(
            !live.contains(&format!("href=\"/threads/{beta}\"")),
            "{live}"
        );
        assert!(
            live.contains("<option value=\"live\" selected>open and active</option>"),
            "{live}"
        );
        let (_, all) = get(shared.clone(), "/threads?status=all").await;
        assert!(all.contains(&format!("href=\"/threads/{beta}\"")), "{all}");
        let (_, resolved) = get(shared.clone(), "/threads?status=resolved").await;
        assert!(
            resolved.contains(&format!("href=\"/threads/{beta}\"")),
            "{resolved}"
        );
        assert!(
            !resolved.contains(&format!("href=\"/threads/{alpha}\"")),
            "{resolved}"
        );

        let (status, invalid) = get(shared.clone(), "/threads?status=bogus").await;
        assert_eq!(status, StatusCode::OK);
        assert!(invalid.contains("error-fragment") && invalid.contains("bbox_thread_list"));
    }

    /// A project the registry learns about between two thread opens: the
    /// first thread is stored under its raw path alone, the second carries
    /// the project id. The filter offers one choice for both, named after
    /// the project and valued by its id, and selects both however the
    /// project is named.
    #[tokio::test]
    async fn project_filter_folds_stored_keys_into_one_named_choice() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let repo = tempfile::tempdir().unwrap();
        let alpha = repo.path().canonicalize().unwrap().join("alpha-repo");
        std::fs::create_dir_all(&alpha).unwrap();
        let alpha = alpha.to_string_lossy().into_owned();

        mutate(
            &shared,
            serde_json::json!({"action": "open", "topic": "Before registration", "project": alpha}),
        );
        let project_id = shared
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&alpha)
            .unwrap()
            .project_id;
        // Stored the way the tool stores a resolved open: the path and the
        // id, which the daemon sets from the resolver and never reads from
        // the wire.
        {
            let mut params: crate::threads::ThreadParams = serde_json::from_value(
                serde_json::json!({"action": "open", "topic": "After registration", "project": alpha}),
            )
            .unwrap();
            params.project_id = Some(project_id.clone());
            shared.threads.write().thread(&params).unwrap();
        }
        mutate(
            &shared,
            serde_json::json!({"action": "open", "topic": "Elsewhere", "project": "/work/elsewhere"}),
        );
        {
            let store = shared.threads.read();
            let keyed = |topic: &str| {
                let thread = store
                    .all()
                    .iter()
                    .find(|thread| thread.topic == topic)
                    .unwrap();
                (thread.project.clone(), thread.project_id.clone())
            };
            assert_eq!(keyed("Before registration"), (alpha.clone(), None));
            assert_eq!(
                keyed("After registration"),
                (alpha.clone(), Some(project_id.clone()))
            );
        }

        let (status, body) = get(shared.clone(), "/threads").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let choice = format!("<option value=\"{project_id}\">alpha-repo</option>");
        assert!(body.contains(&choice), "{body}");
        assert_eq!(
            body.matches("alpha-repo</option>").count(),
            1,
            "one choice for the project, whatever keys its threads carry: {body}"
        );
        assert!(
            !body.contains(&format!("<option value=\"{alpha}\"")),
            "the raw path is not a choice of its own: {body}"
        );
        assert!(
            body.contains("<option value=\"/work/elsewhere\">/work/elsewhere</option>"),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "href=\"/threads?project={project_id}\" title=\"{alpha}\">alpha-repo</a>"
            )),
            "a path-keyed row names the project and keeps its key on hover: {body}"
        );
        assert!(
            body.contains(&format!(
                "href=\"/threads?project={project_id}\" title=\"{project_id}\">alpha-repo</a>"
            )),
            "{body}"
        );

        for value in [project_id.clone(), super::encode_component(&alpha)] {
            let (_, listed) = get(shared.clone(), &format!("/threads?project={value}")).await;
            assert!(
                listed.contains("Before registration") && listed.contains("After registration"),
                "{value}: {listed}"
            );
            assert!(!listed.contains("Elsewhere"), "{value}: {listed}");
            assert!(
                listed.contains(&format!("<option value=\"{project_id}\" selected>")),
                "{value}: {listed}"
            );
        }
        let (_, elsewhere) = get(shared.clone(), "/threads?project=%2Fwork%2Felsewhere").await;
        assert!(elsewhere.contains("Elsewhere") && !elsewhere.contains("registration"));
        // A value no choice covers keeps the tool's substring semantics.
        let (_, substring) = get(shared.clone(), "/threads?project=elsewhere").await;
        assert!(substring.contains("Elsewhere") && !substring.contains("registration"));
        let (_, none) = get(shared.clone(), "/threads?project=nothing-stored").await;
        assert!(none.contains("No threads match these filters."), "{none}");

        let after = {
            let store = shared.threads.read();
            store
                .all()
                .iter()
                .find(|thread| thread.topic == "After registration")
                .unwrap()
                .id
                .clone()
        };
        let (_, detail) = get(shared.clone(), &format!("/threads/{after}")).await;
        assert!(
            detail.contains(&format!("href=\"/threads?project={project_id}\"")),
            "{detail}"
        );
        assert!(detail.contains(">alpha-repo</a>"), "{detail}");
        assert!(
            detail.contains(&format!("href=\"/projects/{project_id}\"")),
            "{detail}"
        );
    }

    #[tokio::test]
    async fn empty_store_names_the_empty_list_and_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (status, body) = get(shared, "/threads").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No threads are stored."));
        assert!(body.contains("bbox_thread_list"));
    }

    #[tokio::test]
    async fn thread_detail_renders_notes_in_order_participants_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (alpha, beta) = seed(&shared);

        let (status, body) = get(shared.clone(), &format!("/threads/{alpha}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("<h1>Audit the roster projection</h1>"),
            "{body}"
        );
        let first = body.find("first note body").expect("first note");
        let second = body.find("second note body").expect("second note");
        assert!(first < second, "notes render in append order");
        assert!(body.contains("planner") && body.contains("sess-planner-1"));
        assert!(body.contains("codex"));
        assert!(body.contains("relates_to"));
        assert!(body.contains(&format!("href=\"/threads/{beta}\"")));
        assert!(body.contains("release depends on the audit"));
        assert!(body.contains("href=\"/threads\" aria-current=\"page\""));

        // A bare hex id resolves like the tool's lookup.
        let bare = alpha.strip_prefix("thread-").unwrap();
        let (status, _) = get(shared.clone(), &format!("/threads/{bare}")).await;
        assert_eq!(status, StatusCode::OK);

        let (status, fragment) = get(shared, &format!("/fragments/threads/{alpha}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!fragment.contains("<html"));
        assert!(fragment.contains("second note body"));
    }

    #[tokio::test]
    async fn unknown_thread_is_a_404_inside_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        seed(&shared);
        let (status, body) = get(shared.clone(), "/threads/thread-00000000").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            body.contains("operator@example.test"),
            "404 renders the shell"
        );
        assert!(body.contains("No thread has the id thread-00000000."));
        let (status, _) = get(shared, "/fragments/threads/thread-00000000").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
