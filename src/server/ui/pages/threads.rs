//! `/threads` and `/threads/{id}`: the thread list with status and project
//! filters, and one thread's notes, participants and linked entities.
//!
//! The list reads through the filter resolution and matcher that
//! `bbox_thread_list` uses; the detail reads the same stored record that
//! `bbox_thread` pages. Both run on the blocking pool: the project resolver
//! may probe the filesystem and the store lock can wait behind a write.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use bbox_threads::threads::{EdgeTarget, Thread, ThreadListParams, ThreadOrigin, ThreadStatus};
use chrono::{DateTime, Utc};
use maud::{Markup, html};
use serde::Deserialize;

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::display::{age, encode_component, parse_rfc3339, preview};

/// Rows per list page.
const PAGE_SIZE: usize = 100;

/// Status filter choices, in display order. `all` lists every status.
const STATUS_CHOICES: [&str; 5] = ["all", "open", "active", "resolved", "promoted"];

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
    fn status(&self) -> Option<&str> {
        self.status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty() && *status != "all")
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

/// The project a thread is keyed to, as `bbox_thread_list` reports it: the
/// resolver's project id when stamped, else the stored selector.
fn thread_project(thread: &Thread) -> Option<&str> {
    thread
        .project_id
        .as_deref()
        .or_else(|| (!thread.project.is_empty()).then_some(thread.project.as_str()))
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
    project: Option<String>,
    notes: usize,
    updated: Option<DateTime<Utc>>,
}

struct ListData {
    rows: Vec<ThreadRow>,
    total: usize,
    /// Every project a stored thread is keyed to, for the filter choices.
    projects: Vec<String>,
    /// Whether the store holds any thread at all.
    store_empty: bool,
}

/// `bbox_thread_list` reader: every status and origin, filtered by the
/// query, newest activity first.
fn read_list(ui: &UiState, query: &ListQuery) -> anyhow::Result<ListData> {
    let (projects, project_ids, store_empty) = {
        let store = ui.shared.threads.read();
        let all = store.all();
        let projects: BTreeSet<String> = all
            .iter()
            .filter_map(|thread| thread_project(thread).map(str::to_string))
            .collect();
        let project_ids: BTreeSet<String> = all
            .iter()
            .filter_map(|thread| thread.project_id.clone())
            .collect();
        (projects, project_ids, all.is_empty())
    };

    let mut filters = ThreadListParams {
        status: query.status().map(str::to_string),
        include_resolved: Some(true),
        include_workflows: Some(true),
        ..ThreadListParams::default()
    };
    if let Some(project) = query.project() {
        if project_ids.contains(project) {
            filters.project_id = Some(project.to_string());
        } else {
            filters.project = Some(project.to_string());
        }
    }
    let filters = ui.server.thread_list_filters(filters);

    let store = ui.shared.threads.read();
    let matching = store.matching_threads(&filters)?;
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
            project: thread_project(thread).map(str::to_string),
            notes: thread.notes.len(),
            updated: parse_rfc3339(&thread.last_activity),
        })
        .collect();
    Ok(ListData {
        rows,
        total,
        projects: projects.into_iter().collect(),
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
            (filter_form(&query, &data.projects))
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

fn filter_form(query: &ListQuery, projects: &[String]) -> Markup {
    let status = query.status().unwrap_or("all");
    let project = query.project();
    html! {
        form.filters method="get" action="/threads" {
            label {
                "Status "
                select name="status" {
                    @for choice in STATUS_CHOICES {
                        option value=(choice) selected[choice == status] { (choice) }
                    }
                }
            }
            label {
                "Project "
                select name="project" {
                    option value="" selected[project.is_none()] { "all projects" }
                    @for candidate in projects {
                        option value=(candidate) selected[Some(candidate.as_str()) == project] {
                            (preview(candidate, 80))
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
                            td {
                                @match &row.project {
                                    Some(project) => { code title=(project) { (preview(project, 60)) } }
                                    None => { span.quiet { "none" } }
                                }
                            }
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

/// `bbox_thread` reader: the stored record for this id.
async fn read_thread(ui: &Arc<UiState>, id: String) -> Result<Option<Thread>, String> {
    let reader_ui = ui.clone();
    tokio::task::spawn_blocking(move || reader_ui.shared.threads.read().thread_by_id(&id).cloned())
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
        Ok(Some(thread)) => (
            StatusCode::OK,
            thread_title(&thread),
            thread_detail(&thread, Utc::now()),
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
        Ok(Some(thread)) => html_response(StatusCode::OK, thread_detail(&thread, Utc::now())),
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

fn thread_detail(thread: &Thread, now: DateTime<Utc>) -> Markup {
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
                                @match thread_project(thread) {
                                    Some(project) => {
                                        a href=(format!("/threads?project={}", encode_component(project))) {
                                            code { (project) }
                                        }
                                    }
                                    None => { span.quiet { "none" } }
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

        let (status, invalid) = get(shared.clone(), "/threads?status=bogus").await;
        assert_eq!(status, StatusCode::OK);
        assert!(invalid.contains("error-fragment") && invalid.contains("bbox_thread_list"));

        let (status, fragment) = get(shared, "/fragments/threads/table?status=open").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!fragment.contains("<html"), "a fragment renders no shell");
        assert!(fragment.contains(&beta) && !fragment.contains(&format!("/threads/{alpha}")));
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
