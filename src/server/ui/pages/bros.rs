//! `/bros` and `/bros/{task}`: the roster and one task's status, dispatch
//! parameters, report and event tail.
//!
//! The roster reads the materialized roster view `bro_dashboard` and
//! `/control/roster` serve, ordered by state bucket (running first) and then
//! by the dashboard's order. The task page reads the task record the status
//! reader projects and compacts retained events with the status reader's
//! event projection. Both pages render fully without JavaScript; two SSE
//! routes keep them fresh:
//!
//! - `/streams/bros` re-renders the roster rows on every roster delta. A
//!   lagged subscriber gets a `resync` event and the page re-fetches
//!   `/fragments/bros/rows`.
//! - `/streams/bros/{task}/tail` appends that task's tail events. It opens
//!   with a `resync` (the page re-fetches the buffered tail, closing the gap
//!   between render and connect), sends `status` and then `finished` when the
//!   task reaches a terminal state, and ends.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use bro_protocol::{RosterSummaryV1, TaskStatus as RosterStatus};
use chrono::{DateTime, Utc};
use futures::Stream;
use maud::{Markup, html};
use serde_json::Value;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};
use super::display::{age, encode_component, from_millis, preview, sse_html};
use super::gaps::{Related, related_panel};
use crate::orchestration::{self, TaskStatus, tail::TailEvent};
use crate::server::BlackboxServer;
use crate::tools::roster::{dashboard_elapsed, dashboard_order};

/// Retained events the task page renders statically.
const TAIL_EVENTS: usize = 50;

/// Largest report body rendered inline; the rest stays behind `bro_status`.
const REPORT_MAX_BYTES: usize = 512 * 1024;

/// Captured stderr shown for a failed task.
const STDERR_TAIL_BYTES: usize = 4 * 1024;

/// Columns of the roster table, for the empty-state row's span.
const ROSTER_COLUMNS: usize = 8;

/// A roster status as its label, state class and bucket rank (running
/// first, finished last).
fn roster_bucket(status: RosterStatus, interrupted: bool) -> (&'static str, &'static str, u8) {
    match status {
        RosterStatus::Running => ("running", "running", 0),
        RosterStatus::Pending => ("pending", "waiting", 1),
        RosterStatus::Failed => ("failed", "failed", 2),
        RosterStatus::Cancelled if interrupted => ("interrupted", "finished", 3),
        RosterStatus::Cancelled => ("cancelled", "finished", 3),
        RosterStatus::Completed => ("completed", "finished", 4),
    }
}

fn task_bucket(status: TaskStatus, interrupted: bool) -> (&'static str, &'static str) {
    match status {
        TaskStatus::Running => ("running", "running"),
        TaskStatus::Failed => ("failed", "failed"),
        TaskStatus::Cancelled if interrupted => ("interrupted", "finished"),
        TaskStatus::Cancelled => ("cancelled", "finished"),
        TaskStatus::Completed => ("completed", "finished"),
    }
}

fn state_label(label: &str, state: &str) -> Markup {
    html! { span class=(format!("state state-{state}")) { (label) } }
}

fn task_href(task_id: &str) -> String {
    format!("/bros/{}", encode_component(task_id))
}

// ── Roster ──────────────────────────────────────────────────────────

/// The roster's query string: finished tasks are opt-in.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct RosterQuery {
    #[serde(default)]
    finished: Option<String>,
}

impl RosterQuery {
    fn include_finished(&self) -> bool {
        matches!(
            self.finished.as_deref().map(str::trim),
            Some("1") | Some("true") | Some("yes")
        )
    }
}

/// The roster as the page lists it: the rows to show, running first, and
/// how many finished tasks the view holds (shown or hidden).
struct Roster {
    rows: Vec<RosterSummaryV1>,
    finished: usize,
    include_finished: bool,
}

/// `bro_dashboard` reader: the roster view, running tasks first. Finished
/// tasks (completed, failed, cancelled) are listed only when asked for.
fn roster_rows(ui: &UiState, include_finished: bool) -> Roster {
    let mut rows = ui.shared.roster_view.snapshot();
    let finished = rows.iter().filter(|s| s.status.is_terminal()).count();
    if !include_finished {
        rows.retain(|s| !s.status.is_terminal());
    }
    rows.sort_by(|a, b| {
        let rank = |s: &RosterSummaryV1| roster_bucket(s.status, s.interrupted).2;
        rank(a).cmp(&rank(b)).then_with(|| dashboard_order(a, b))
    });
    Roster {
        rows,
        finished,
        include_finished,
    }
}

/// The roster routes with the finished flag carried along.
fn roster_href(path: &str, include_finished: bool) -> String {
    if include_finished {
        format!("{path}?finished=1")
    } else {
        path.to_string()
    }
}

/// The roster table body: one row per task, or one empty-state row so a
/// live update has a body to replace.
fn render_rows(rows: &[RosterSummaryV1], now: DateTime<Utc>) -> Markup {
    html! {
        @if rows.is_empty() {
            tr.roster-empty {
                td colspan=(ROSTER_COLUMNS) {
                    (empty_state("No tasks are on the roster.", "bro_dashboard"))
                }
            }
        }
        @for row in rows {
            @let (label, state, _) = roster_bucket(row.status, row.interrupted);
            @let task_id = row.task_id.as_str();
            tr {
                td { (state_label(label, state)) }
                td { a href=(task_href(task_id)) { code { (task_id) } } }
                td {
                    @match &row.label {
                        Some(bro) => { (bro) }
                        None => { span.quiet { "ad hoc" } }
                    }
                }
                td.roster-name {
                    @match &row.name {
                        Some(name) => { span title=(name) { (preview(name, 80)) } }
                        None => { span.quiet { "unnamed" } }
                    }
                }
                td { (row.provider.as_str()) }
                td {
                    @match &row.model {
                        Some(model) => { code { (model) } }
                        None => { span.quiet { "default" } }
                    }
                }
                td.num { (dashboard_elapsed(row)) }
                td { (age(row.last_event_at.and_then(from_millis), now)) }
            }
        }
    }
}

fn render_roster(roster: &Roster, now: DateTime<Utc>) -> Markup {
    let include_finished = roster.include_finished;
    html! {
        p.roster-toggle {
            @if include_finished {
                a href="/bros" { "Hide finished tasks" }
            } @else if roster.finished == 0 {
                span.muted { "No finished tasks on the roster." }
            } @else {
                a href="/bros?finished=1" {
                    "Show " (roster.finished) " finished "
                    @if roster.finished == 1 { "task" } @else { "tasks" }
                }
            }
        }
        div.roster-live hx-ext="sse" sse-connect=(roster_href("/streams/bros", include_finished)) {
            table.roster-table {
                thead {
                    tr {
                        th scope="col" { "State" }
                        th scope="col" { "Task" }
                        th scope="col" { "Bro" }
                        th scope="col" { "Name" }
                        th scope="col" { "Provider" }
                        th scope="col" { "Model" }
                        th.num scope="col" { "Elapsed" }
                        th scope="col" { "Last event" }
                    }
                }
                tbody id="roster-rows" sse-swap="roster" hx-swap="innerHTML"
                    hx-get=(roster_href("/fragments/bros/rows", include_finished)) hx-trigger="sse:resync" {
                    (render_rows(&roster.rows, now))
                }
            }
        }
        p.panel-source {
            "Source: " code { "bro_dashboard" } ", kept live by the roster stream."
        }
    }
}

/// `GET /bros`
pub(crate) async fn roster_page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<RosterQuery>,
) -> Response {
    let content = render_roster(&roster_rows(&ui, query.include_finished()), Utc::now());
    html_response(
        StatusCode::OK,
        layout(&ui.shell(&principal), Some(Section::Bros), "Bros", content),
    )
}

/// `GET /fragments/bros/rows`: the roster table body.
pub(crate) async fn rows_fragment(
    State(ui): State<Arc<UiState>>,
    Query(query): Query<RosterQuery>,
) -> Response {
    html_response(
        StatusCode::OK,
        render_rows(&roster_rows(&ui, query.include_finished()).rows, Utc::now()),
    )
}

fn rows_event(ui: &UiState, include_finished: bool) -> Event {
    Event::default().event("roster").data(sse_html(render_rows(
        &roster_rows(ui, include_finished).rows,
        Utc::now(),
    )))
}

fn resync_event(skipped: u64) -> Event {
    Event::default().event("resync").data(skipped.to_string())
}

/// `GET /streams/bros`: the roster rows, re-rendered from the roster view on
/// every delta. The view is written before a delta is broadcast, so the rows
/// rendered for a delta include it. Deltas that queued while a render ran
/// are folded into the next render.
pub(crate) async fn roster_stream(
    State(ui): State<Arc<UiState>>,
    Query(query): Query<RosterQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let include_finished = query.include_finished();
    let mut rx = ui.shared.roster_tx.subscribe();
    let stream = async_stream::stream! {
        yield Ok(rows_event(&ui, include_finished));
        loop {
            match rx.recv().await {
                Ok(_) => {
                    while let Ok(_) | Err(TryRecvError::Lagged(_)) = rx.try_recv() {}
                    yield Ok(rows_event(&ui, include_finished));
                }
                Err(RecvError::Lagged(skipped)) => yield Ok(resync_event(skipped)),
                Err(RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Task detail ─────────────────────────────────────────────────────

/// What the task page shows, read under the task's lock in one pass.
struct TaskDetail {
    id: String,
    status: TaskStatus,
    interrupted: bool,
    recoverable: bool,
    name: Option<String>,
    provider: &'static str,
    model: Option<String>,
    bro_label: Option<String>,
    origin: String,
    session_id: String,
    cwd: Option<String>,
    managed_worktree: Option<String>,
    project_id: Option<String>,
    harness_bin: Option<String>,
    workflow_owned: bool,
    started_at: u64,
    completed_at: Option<u64>,
    elapsed: String,
    exit_code: Option<i32>,
    cost_usd: Option<f64>,
    num_turns: Option<u64>,
    report: Option<String>,
    report_bytes: usize,
    latest_preview: Option<String>,
    stderr_tail: Option<String>,
    event_count: usize,
    retained_events: usize,
    /// The last retained events, oldest first, compacted by the status
    /// reader's event projection and credential-redacted.
    events: Vec<Value>,
}

/// An event as the page shows it: the status reader's compaction, then the
/// configuration credential redaction (environment and header values).
/// `None` for stream partials the status reader drops.
fn displayable_event(event: &Value) -> Option<Value> {
    orchestration::compact_status_event(event)
        .map(|event| BlackboxServer::redact_config_credentials(&event))
}

/// `bro_status` reader: the task record, or `None` for an unknown id.
fn read_task(ui: &UiState, task_id: &str) -> Option<TaskDetail> {
    let task = ui.shared.task_store.read().get(task_id)?;
    let inner = task.inner.lock();
    let mut events: Vec<Value> = inner
        .events
        .iter()
        .rev()
        .filter_map(displayable_event)
        .take(TAIL_EVENTS)
        .collect();
    events.reverse();
    let terminal = inner.status.is_terminal();
    let report = inner
        .last_assistant_message
        .as_deref()
        .filter(|_| terminal)
        .map(|body| {
            let mut end = body.len().min(REPORT_MAX_BYTES);
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body[..end].to_string()
        });
    let stderr = inner.stderr.trim_end();
    Some(TaskDetail {
        id: inner.id.clone(),
        status: inner.status,
        interrupted: inner.interrupted,
        recoverable: inner.recoverable,
        name: inner.name.clone(),
        provider: inner.provider.as_str(),
        model: inner.model.clone(),
        bro_label: inner.bro_label.clone(),
        origin: inner.origin.as_wire().to_string(),
        session_id: inner.session_id.clone(),
        cwd: inner.cwd.clone(),
        managed_worktree: inner.managed_worktree.clone(),
        project_id: inner.project_id.clone(),
        harness_bin: inner.harness_bin.clone(),
        workflow_owned: inner.workflow_owned,
        started_at: inner.started_at,
        completed_at: inner.completed_at,
        elapsed: orchestration::format_elapsed(inner.started_at, inner.completed_at),
        exit_code: inner.exit_code,
        cost_usd: inner.cost_usd,
        num_turns: inner.num_turns,
        report_bytes: inner
            .last_assistant_message
            .as_deref()
            .filter(|_| terminal)
            .map_or(0, str::len),
        report,
        latest_preview: (!terminal)
            .then(|| inner.latest_assistant_preview.text().map(str::to_string))
            .flatten(),
        stderr_tail: (inner.status == TaskStatus::Failed && !stderr.trim().is_empty())
            .then(|| orchestration::tail_str_safe(stderr, STDERR_TAIL_BYTES)),
        event_count: inner.observed_event_count(),
        retained_events: inner.events.retained_len(),
        events,
    })
}

async fn task_detail(ui: &Arc<UiState>, task_id: String) -> Result<Option<TaskDetail>, String> {
    let reader_ui = ui.clone();
    tokio::task::spawn_blocking(move || read_task(&reader_ui, &task_id))
        .await
        .map_err(|error| error.to_string())
}

fn missing_task(task_id: &str) -> Markup {
    html! {
        (empty_state(&format!("No task has the id {task_id}."), "bro_status"))
        p { a href="/bros" { "Roster" } }
    }
}

fn param_row(name: &str, value: Option<&str>) -> Markup {
    html! {
        tr {
            th scope="row" { (name) }
            td {
                @match value {
                    Some(value) => { code { (value) } }
                    None => { span.quiet { "none" } }
                }
            }
        }
    }
}

/// Status, dispatch parameters and report: the pane the task page renders
/// and `/fragments/bros/{task}/summary` returns.
fn render_summary(task: &TaskDetail, now: DateTime<Utc>) -> Markup {
    let (label, state) = task_bucket(task.status, task.interrupted);
    html! {
        div.panels.task-panels {
            section.panel aria-labelledby="task-status" {
                h2 id="task-status" { "Status" }
                table.detail-table {
                    tbody {
                        tr { th scope="row" { "State" } td { (state_label(label, state)) } }
                        tr { th scope="row" { "Task" } td { code { (task.id) } } }
                        @if let Some(name) = &task.name {
                            tr { th scope="row" { "Name" } td.prose { (name) } }
                        }
                        tr { th scope="row" { "Elapsed" } td.num { (task.elapsed) } }
                        tr { th scope="row" { "Started" } td { (age(from_millis(task.started_at), now)) } }
                        @if task.status.is_terminal() {
                            tr { th scope="row" { "Finished" } td { (age(task.completed_at.and_then(from_millis), now)) } }
                        }
                        @if let Some(code) = task.exit_code.filter(|_| task.status.is_terminal()) {
                            tr { th scope="row" { "Exit code" } td.num { (code) } }
                        }
                        @if let Some(turns) = task.num_turns {
                            tr { th scope="row" { "Turns" } td.num { (turns) } }
                        }
                        @if let Some(cost) = task.cost_usd {
                            tr { th scope="row" { "Cost" } td.num { (format!("${cost:.2}")) } }
                        }
                        tr {
                            th scope="row" { "Events" }
                            td.num { (task.event_count) " observed, " (task.retained_events) " retained" }
                        }
                        @if task.recoverable {
                            tr { th scope="row" { "Recoverable" } td { "yes: resume the session to continue" } }
                        }
                    }
                }
            }
            section.panel aria-labelledby="task-dispatch" {
                h2 id="task-dispatch" { "Dispatch parameters" }
                table.detail-table {
                    tbody {
                        (param_row("Provider", Some(task.provider)))
                        (param_row("Model", task.model.as_deref()))
                        (param_row("Bro", task.bro_label.as_deref()))
                        (param_row("Origin", Some(task.origin.as_str())))
                        (param_row("Session", Some(&task.session_id).filter(|id| !id.is_empty()).map(String::as_str)))
                        (param_row("Working directory", task.cwd.as_deref()))
                        (param_row("Managed worktree", task.managed_worktree.as_deref()))
                        (param_row("Project", task.project_id.as_deref()))
                        (param_row("Harness binary", task.harness_bin.as_deref()))
                        tr {
                            th scope="row" { "Workflow owned" }
                            td { (if task.workflow_owned { "yes" } else { "no" }) }
                        }
                    }
                }
                p.panel-source {
                    "The task record keeps no environment, token or header values. "
                    "Event payloads below have environment and header values redacted. Source: "
                    code { "bro_status" }
                }
            }
        }
        section.panel aria-labelledby="task-report" {
            h2 id="task-report" { "Report" }
            @if let Some(report) = &task.report {
                pre.task-report { (report) }
                @if task.report_bytes > report.len() {
                    p.panel-source {
                        "Showing the first " (report.len()) " of " (task.report_bytes)
                        " bytes. Read the rest with " code { "bro_status(detail=result)" } "."
                    }
                }
            } @else if let Some(preview) = &task.latest_preview {
                p.quiet { "The task is still running. Latest assistant text:" }
                pre.task-report { (preview) }
            } @else if task.status.is_terminal() {
                (empty_state("The task finished without a captured report.", "bro_status"))
            } @else {
                (empty_state("The task has not produced assistant text yet.", "bro_status"))
            }
            @if let Some(stderr) = &task.stderr_tail {
                h2 { "Error output" }
                pre.task-report { (stderr) }
            }
        }
    }
}

/// A one-line summary of a retained or live event: its type and the text
/// or tool names it carries.
fn summarize_event(event: &Value) -> (String, String) {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("event")
        .to_string();
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = event
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    {
        for item in content {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
                Some("tool_use") => parts.push(format!(
                    "tool {}",
                    item.get("name").and_then(Value::as_str).unwrap_or("call")
                )),
                Some("tool_result") => parts.push("tool result".to_string()),
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        for key in ["result", "text", "subtype"] {
            if let Some(text) = event.get(key).and_then(Value::as_str) {
                parts.push(text.to_string());
                break;
            }
        }
    }
    (kind, preview(&parts.join(" | "), 300))
}

fn event_item(event: &Value) -> Markup {
    let (kind, text) = summarize_event(event);
    let json = serde_json::to_string_pretty(event).unwrap_or_default();
    html! {
        li.tail-event {
            span.tail-kind { (kind) }
            span.tail-text { (text) }
            details {
                summary { "event" }
                pre { (json) }
            }
        }
    }
}

fn lifecycle_item(kind: &str, state: &str, text: &str) -> Markup {
    html! {
        li.tail-event.tail-lifecycle {
            span.tail-kind { (state_label(kind, state)) }
            span.tail-text { (text) }
        }
    }
}

/// One live tail event as a list item; `None` for a stream partial.
fn tail_item(event: &TailEvent) -> Option<Markup> {
    Some(match event {
        TailEvent::TaskStarted {
            provider, bro_name, ..
        } => lifecycle_item(
            "started",
            "running",
            &match bro_name {
                Some(bro) => format!("{} as {bro}", provider.as_str()),
                None => provider.as_str().to_string(),
            },
        ),
        TailEvent::TaskProgress { activity, .. } => lifecycle_item("progress", "running", activity),
        TailEvent::TaskEvent { event, .. } => event_item(&displayable_event(event)?),
        TailEvent::TaskCompleted { elapsed, cost, .. } => lifecycle_item(
            "completed",
            "finished",
            &match cost {
                Some(cost) => format!("after {elapsed}, ${cost:.2}"),
                None => format!("after {elapsed}"),
            },
        ),
        TailEvent::TaskFailed { elapsed, error, .. } => {
            lifecycle_item("failed", "failed", &format!("after {elapsed}: {error}"))
        }
        TailEvent::TaskCancelled { elapsed, .. } => {
            lifecycle_item("cancelled", "finished", &format!("after {elapsed}"))
        }
    })
}

/// The buffered tail: the page's static list and
/// `/fragments/bros/{task}/tail`. The empty state hides once a live event
/// lands beside it.
fn render_tail_items(task: &TaskDetail) -> Markup {
    html! {
        li.tail-empty {
            (empty_state("No events are retained for this task.", "bro_status"))
        }
        @for event in &task.events {
            (event_item(event))
        }
    }
}

fn render_task(task: &TaskDetail, now: DateTime<Utc>) -> Markup {
    let href = encode_component(&task.id);
    let live = !task.status.is_terminal();
    let tail = html! {
        section.panel aria-labelledby="task-tail-heading" {
            h2 id="task-tail-heading" { "Event tail" }
            div hx-get=(format!("/fragments/bros/{href}/tail")) hx-trigger="sse:resync"
                hx-target="#task-tail" hx-swap="innerHTML" {
                ol.tail id="task-tail" sse-swap="tail" hx-swap="beforeend" {
                    (render_tail_items(task))
                }
            }
            p.panel-source {
                @if live {
                    "The last " (TAIL_EVENTS) " retained events, then live events as they arrive. "
                } @else {
                    "The last " (TAIL_EVENTS) " retained events; the task has finished. "
                }
                "Source: " code { "bro_status" }
            }
        }
    };
    let summary = html! {
        div id="task-summary" hx-get=(format!("/fragments/bros/{href}/summary"))
            hx-trigger="sse:status" hx-swap="innerHTML" {
            (render_summary(task, now))
        }
    };
    html! {
        p { a href="/bros" { "Roster" } }
        @if live {
            div.task-live hx-ext="sse" sse-connect=(format!("/streams/bros/{href}/tail"))
                sse-close="finished" {
                (summary)
                (tail)
            }
        } @else {
            (summary)
            (tail)
        }
    }
}

/// The panel of gaps this task filed, read on the blocking pool.
async fn task_gaps(ui: &Arc<UiState>, task_id: String) -> Markup {
    let reader_ui = ui.clone();
    tokio::task::spawn_blocking(move || related_panel(&reader_ui, Related::Task(&task_id)))
        .await
        .unwrap_or_else(|error| error_fragment("bbox_gaps", &error.to_string()))
}

/// `GET /bros/{task}`
pub(crate) async fn task_page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
    Path(task_id): Path<String>,
) -> Response {
    let (status, title, content) = match task_detail(&ui, task_id.clone()).await {
        Ok(Some(task)) => {
            let gaps = task_gaps(&ui, task.id.clone()).await;
            (
                StatusCode::OK,
                format!("Task {}", task.id),
                html! { (render_task(&task, Utc::now())) (gaps) },
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            "Task not found".to_string(),
            missing_task(&task_id),
        ),
        Err(error) => (
            StatusCode::OK,
            "Task".to_string(),
            error_fragment("bro_status", &error),
        ),
    };
    html_response(
        status,
        layout(&ui.shell(&principal), Some(Section::Bros), &title, content),
    )
}

/// `GET /fragments/bros/{task}/summary`
pub(crate) async fn summary_fragment(
    State(ui): State<Arc<UiState>>,
    Path(task_id): Path<String>,
) -> Response {
    match task_detail(&ui, task_id.clone()).await {
        Ok(Some(task)) => html_response(StatusCode::OK, render_summary(&task, Utc::now())),
        Ok(None) => html_response(StatusCode::NOT_FOUND, missing_task(&task_id)),
        Err(error) => html_response(StatusCode::OK, error_fragment("bro_status", &error)),
    }
}

/// `GET /fragments/bros/{task}/tail`
pub(crate) async fn tail_fragment(
    State(ui): State<Arc<UiState>>,
    Path(task_id): Path<String>,
) -> Response {
    match task_detail(&ui, task_id.clone()).await {
        Ok(Some(task)) => html_response(StatusCode::OK, render_tail_items(&task)),
        Ok(None) => html_response(StatusCode::NOT_FOUND, missing_task(&task_id)),
        Err(error) => html_response(StatusCode::OK, error_fragment("bro_status", &error)),
    }
}

/// `GET /streams/bros/{task}/tail`: that task's tail events as list items.
pub(crate) async fn task_stream(
    State(ui): State<Arc<UiState>>,
    Path(task_id): Path<String>,
) -> Response {
    if ui.shared.task_store.read().get(&task_id).is_none() {
        return html_response(StatusCode::NOT_FOUND, missing_task(&task_id));
    }
    let mut rx = ui.shared.tail_tx.subscribe();
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(resync_event(0));
        loop {
            match rx.recv().await {
                Ok(event) if event.task_id() == task_id => {
                    if let Some(item) = tail_item(&event) {
                        yield Ok(Event::default().event("tail").data(sse_html(item)));
                    }
                    if matches!(
                        event,
                        TailEvent::TaskCompleted { .. }
                            | TailEvent::TaskFailed { .. }
                            | TailEvent::TaskCancelled { .. }
                    ) {
                        yield Ok(Event::default().event("status").data("terminal"));
                        yield Ok(Event::default().event("finished").data("finished"));
                        break;
                    }
                }
                Ok(_) => continue,
                Err(RecvError::Lagged(skipped)) => yield Ok(resync_event(skipped)),
                Err(RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::super::super::{UiState, build_ui_router};
    use crate::orchestration::{self, TaskStatus, providers::Provider};
    use crate::server::SharedState;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use futures::StreamExt;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    const IDENTITY: (&str, &str) = ("x-forwarded-email", "operator@example.test");

    fn request(uri: &str, identity: bool) -> Request<Body> {
        let mut builder = Request::builder()
            .uri(uri)
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                53200,
            )));
        if identity {
            builder = builder.header(IDENTITY.0, IDENTITY.1);
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn get(shared: Arc<SharedState>, uri: &str) -> (StatusCode, String) {
        let response = build_ui_router(UiState::new(shared))
            .oneshot(request(uri, true))
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn summary(
        task: &str,
        status: &str,
        label: Option<&str>,
        started_at: u64,
    ) -> bro_protocol::RosterSummaryV1 {
        serde_json::from_value(serde_json::json!({
            "task_id": task,
            "status": status,
            "provider": Provider::Glm,
            "label": label,
            "name": format!("Name of {task}"),
            "model": "glm-pro",
            "started_at": started_at,
            "last_event_at": started_at + 1_500,
        }))
        .unwrap()
    }

    /// A running task with retained events (one carrying a credential in an
    /// environment map) and a completed task with a report. Both are on the
    /// roster view and in the task store.
    fn seed(shared: &SharedState) -> Arc<orchestration::Task> {
        let running = orchestration::test_task("t-run", TaskStatus::Running, Provider::Glm);
        {
            let mut inner = running.inner.lock();
            inner.completed_at = None;
            inner.bro_label = Some("executor".to_string());
            inner.model = Some("glm-pro".to_string());
            inner.cwd = Some("/work/alpha".to_string());
        }
        orchestration::push_in_process_event(
            &running,
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": "retained hello"}]},
            }),
            &shared.tail_tx,
        );
        orchestration::push_in_process_event(
            &running,
            serde_json::json!({
                "type": "tool_use",
                "input": {"env": {"API_TOKEN": "secret-sentinel-value"}},
            }),
            &shared.tail_tx,
        );
        let done = orchestration::test_task("t-done", TaskStatus::Completed, Provider::Glm);
        done.inner.lock().last_assistant_message = Some("Final <report> body".to_string());
        let mut store = shared.task_store.write();
        store.insert("t-run".to_string(), running.clone()).unwrap();
        store.insert("t-done".to_string(), done).unwrap();
        drop(store);
        shared.roster_view.upsert(
            "t-done".to_string(),
            summary("t-done", "completed", Some("reviewer"), 1_700_000_100_000),
        );
        shared.roster_view.upsert(
            "t-run".to_string(),
            summary("t-run", "running", Some("executor"), 1_700_000_000_000),
        );
        running
    }

    /// Read SSE body chunks until the accumulated text contains `needle`.
    async fn read_until(
        body: &mut (impl futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin),
        seen: &mut String,
        needle: &str,
    ) {
        while !seen.contains(needle) {
            let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; saw {seen}"))
                .expect("stream ended")
                .unwrap();
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    #[tokio::test]
    async fn roster_lists_seeded_tasks_running_first_with_links() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        seed(&shared);

        // Finished tasks are opt-in: the default roster shows the running
        // task and offers the finished one behind a link.
        let (status, live) = get(shared.clone(), "/bros").await;
        assert_eq!(status, StatusCode::OK, "{live}");
        assert!(live.contains("href=\"/bros/t-run\""), "{live}");
        assert!(!live.contains("href=\"/bros/t-done\""), "{live}");
        assert!(live.contains("Show 1 finished task"), "{live}");
        assert!(live.contains("sse-connect=\"/streams/bros\""));

        let (status, body) = get(shared.clone(), "/bros?finished=1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("href=\"/bros\" aria-current=\"page\""));
        assert!(body.contains("href=\"/bros/t-run\""), "{body}");
        assert!(body.contains("href=\"/bros/t-done\""));
        assert!(body.contains("Hide finished tasks"), "{body}");
        assert!(body.contains("<span class=\"state state-running\">running</span>"));
        assert!(body.contains("<span class=\"state state-finished\">completed</span>"));
        assert!(body.contains("executor") && body.contains("reviewer"));
        assert!(body.contains("glm-pro") && body.contains(">glm<"));
        assert!(
            body.contains("<td class=\"num\">1s</td>"),
            "terminal elapsed: {body}"
        );
        assert!(body.contains("sse-connect=\"/streams/bros?finished=1\""));
        assert!(body.contains("hx-get=\"/fragments/bros/rows?finished=1\""));
        // The completed task started later, but running tasks come first.
        assert!(body.find("/bros/t-run").unwrap() < body.find("/bros/t-done").unwrap());

        let (status, rows) = get(shared.clone(), "/fragments/bros/rows").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!rows.contains("<html") && !rows.contains("<table"));
        assert!(rows.contains("href=\"/bros/t-run\""));
        assert!(!rows.contains("href=\"/bros/t-done\""));
        let (_, rows) = get(shared, "/fragments/bros/rows?finished=1").await;
        assert!(rows.contains("href=\"/bros/t-done\""));
    }

    #[tokio::test]
    async fn empty_roster_renders_an_empty_state_row() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let (status, body) = get(shared, "/bros").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No tasks are on the roster."));
        assert!(body.contains("id=\"roster-rows\""));
    }

    #[tokio::test]
    async fn task_page_renders_status_parameters_report_and_redacted_tail() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        seed(&shared);

        let (status, body) = get(shared.clone(), "/bros/t-run").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("<h1>Task t-run</h1>"));
        assert!(body.contains("Dispatch parameters"));
        assert!(body.contains("/work/alpha") && body.contains("executor"));
        assert!(
            body.contains("retained hello"),
            "retained events render statically"
        );
        assert!(body.contains("API_TOKEN"), "credential keys stay visible");
        assert!(
            !body.contains("secret-sentinel-value"),
            "credential values are redacted"
        );
        assert!(body.contains("sse-connect=\"/streams/bros/t-run/tail\""));

        let (status, done) = get(shared.clone(), "/bros/t-done").await;
        assert_eq!(status, StatusCode::OK);
        assert!(done.contains("Final &lt;report&gt; body"), "{done}");
        assert!(
            !done.contains("sse-connect"),
            "a finished task opens no stream"
        );
        assert!(done.contains("No events are retained for this task."));

        let (status, pane) = get(shared.clone(), "/fragments/bros/t-done/summary").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!pane.contains("<html") && pane.contains("Final &lt;report&gt; body"));
        let (status, tail) = get(shared, "/fragments/bros/t-run/tail").await;
        assert_eq!(status, StatusCode::OK);
        assert!(tail.contains("retained hello") && !tail.contains("<html"));
    }

    #[tokio::test]
    async fn unknown_task_is_a_404_inside_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        seed(&shared);
        let (status, body) = get(shared.clone(), "/bros/no-such-task").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("operator@example.test"));
        assert!(body.contains("No task has the id no-such-task."));
        for uri in [
            "/fragments/bros/no-such-task/summary",
            "/fragments/bros/no-such-task/tail",
            "/streams/bros/no-such-task/tail",
        ] {
            let (status, _) = get(shared.clone(), uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn stream_routes_refuse_a_request_without_identity() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        seed(&shared);
        for uri in [
            "/streams/bros",
            "/streams/bros/t-run/tail",
            "/fragments/bros/rows",
            "/bros/t-run",
        ] {
            let response = build_ui_router(UiState::new(shared.clone()))
                .oneshot(request(uri, false))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    #[tokio::test]
    async fn roster_stream_emits_rows_when_a_delta_is_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let response = build_ui_router(UiState::new(shared.clone()))
            .oneshot(request("/streams/bros", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let mut body = response.into_body().into_data_stream();
        let mut seen = String::new();
        read_until(&mut body, &mut seen, "No tasks are on the roster.").await;
        assert!(seen.contains("event: roster"), "{seen}");

        let task = summary("t-new", "running", Some("executor"), 1_700_000_000_000);
        shared.roster_view.upsert("t-new".to_string(), task.clone());
        shared
            .roster_tx
            .send(bro_protocol::RosterDelta::Added { seq: 1, task })
            .unwrap();
        let mut after = String::new();
        read_until(&mut body, &mut after, "/bros/t-new").await;
        assert!(after.contains("event: roster"), "{after}");
        assert!(after.contains("state-running"));
    }

    #[tokio::test]
    async fn task_stream_appends_live_events_for_that_task_only() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let running = seed(&shared);
        let other = orchestration::test_task("t-other", TaskStatus::Running, Provider::Glm);

        let response = build_ui_router(UiState::new(shared.clone()))
            .oneshot(request("/streams/bros/t-run/tail", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let mut seen = String::new();
        read_until(&mut body, &mut seen, "event: resync").await;

        orchestration::push_in_process_event(
            &other,
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "other task"}]}}),
            &shared.tail_tx,
        );
        orchestration::push_in_process_event(
            &running,
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "live hello"}]}}),
            &shared.tail_tx,
        );
        let mut after = String::new();
        read_until(&mut body, &mut after, "live hello").await;
        assert!(after.contains("event: tail"), "{after}");
        assert!(!after.contains("other task"), "{after}");

        shared
            .tail_tx
            .send(orchestration::tail::TailEvent::TaskCompleted {
                cursor: 9,
                task_id: "t-run".to_string(),
                elapsed: "3s".to_string(),
                cost: None,
                source_session: "sess-t-run".to_string(),
                task_kind: None,
            })
            .unwrap();
        let mut done = String::new();
        read_until(&mut body, &mut done, "event: finished").await;
        assert!(
            done.contains("after 3s") && done.contains("event: status"),
            "{done}"
        );
        let end = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("the stream ends after a terminal event");
        assert!(end.is_none());
    }
}
