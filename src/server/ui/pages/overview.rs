//! `/`: daemon overview. Counts of threads by status, bros by task state and
//! registered projects, index health and open gaps, each read through the
//! same internal reader as its MCP tool.

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use bbox_gaps::gaps::GapResolution;
use bbox_threads::threads::ThreadStatus;
use bro_protocol::TaskStatus;
use maud::{Markup, html};

use super::super::shell::{Section, empty_state, error_fragment, layout};
use super::super::{Principal, UiState, html_response};

/// One labelled count, with the state class its label renders under.
struct Count {
    label: &'static str,
    state: Option<&'static str>,
    value: usize,
}

struct OverviewData {
    /// `bbox_thread_list` reader: the thread store.
    threads: Vec<Count>,
    /// `bro_dashboard` reader: the materialized roster view.
    bros: Vec<Count>,
    /// `bbox_project_list` reader: the project records snapshot.
    projects: Vec<Count>,
    /// `bbox_stats` reader: the index statistics.
    index: Result<Vec<(String, String)>, String>,
    /// `bbox_gaps` reader: the session gap view, open records only.
    gaps: Result<Vec<Count>, String>,
}

impl OverviewData {
    /// Read every region. Runs on the blocking pool: the index statistics
    /// and the gap view may touch the filesystem.
    fn read(ui: &UiState) -> Self {
        let shared = &ui.shared;

        let threads = {
            let store = shared.threads.read();
            let all = store.all();
            [
                (ThreadStatus::Open, "open", Some("waiting")),
                (ThreadStatus::Active, "active", Some("running")),
                (ThreadStatus::Resolved, "resolved", Some("finished")),
                (ThreadStatus::Promoted, "promoted", Some("finished")),
            ]
            .into_iter()
            .map(|(status, label, state)| Count {
                label,
                state,
                value: all.iter().filter(|thread| thread.status == status).count(),
            })
            .collect()
        };

        let roster = shared.roster_view.snapshot();
        let bros = [
            (TaskStatus::Pending, "pending", "waiting"),
            (TaskStatus::Running, "running", "running"),
            (TaskStatus::Completed, "completed", "finished"),
            (TaskStatus::Failed, "failed", "failed"),
            (TaskStatus::Cancelled, "cancelled", "finished"),
        ]
        .into_iter()
        .map(|(status, label, state)| Count {
            label,
            state: Some(state),
            value: roster.iter().filter(|task| task.status == status).count(),
        })
        .collect();

        let records = shared.records_provider.records_snapshot();
        let projects = vec![
            Count {
                label: "registered roots",
                state: None,
                value: records.records.len(),
            },
            Count {
                label: "corpus projects",
                state: None,
                value: records.corpus_project_ids.len(),
            },
        ];

        let index = shared
            .idx
            .read()
            .stats()
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| match line.split_once(": ") {
                        Some((key, value)) => (key.to_string(), value.to_string()),
                        None => (line.to_string(), String::new()),
                    })
                    .collect()
            })
            .map_err(|error| format!("{error:#}"));

        let gaps = ui
            .server
            .session_gap_view(None)
            .map(|view| {
                let all = view.gaps.all();
                [
                    (GapResolution::Unresolved, "unresolved", "waiting"),
                    (GapResolution::Acknowledged, "acknowledged", "stale"),
                ]
                .into_iter()
                .map(|(resolution, label, state)| Count {
                    label,
                    state: Some(state),
                    value: all
                        .iter()
                        .filter(|gap| gap.resolution == resolution)
                        .count(),
                })
                .collect()
            })
            .map_err(|error| format!("{error:#}"));

        Self {
            threads,
            bros,
            projects,
            index,
            gaps,
        }
    }
}

/// `GET /`
pub(crate) async fn page(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let reader_ui = ui.clone();
    let content = match tokio::task::spawn_blocking(move || OverviewData::read(&reader_ui)).await {
        Ok(data) => render(&data),
        Err(error) => error_fragment("overview", &error.to_string()),
    };
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(&principal),
            Some(Section::Overview),
            "Overview",
            content,
        ),
    )
}

fn render(data: &OverviewData) -> Markup {
    html! {
        div.panels {
            section.panel aria-labelledby="overview-threads" {
                h2 id="overview-threads" { a href="/threads" { "Threads" } }
                (count_table("Status", &data.threads, "No threads are stored.", "bbox_thread_list"))
            }
            section.panel aria-labelledby="overview-bros" {
                h2 id="overview-bros" { a href="/bros" { "Bros" } }
                (count_table("State", &data.bros, "No tasks are on the roster.", "bro_dashboard"))
            }
            section.panel aria-labelledby="overview-projects" {
                h2 id="overview-projects" { a href="/projects" { "Projects" } }
                (count_table("Kind", &data.projects, "No projects are registered.", "bbox_project_list"))
            }
            section.panel aria-labelledby="overview-index" {
                h2 id="overview-index" { a href="/system" { "Index" } }
                @match &data.index {
                    Ok(rows) => {
                        @if rows.is_empty() {
                            (empty_state("The index reported no statistics.", "bbox_stats"))
                        } @else {
                        table {
                            tbody {
                                @for (key, value) in rows {
                                    tr { th scope="row" { (key) } td { (value) } }
                                }
                            }
                        }
                        p.panel-source { "Source: " code { "bbox_stats" } }
                        }
                    }
                    Err(error) => { (error_fragment("bbox_stats", error)) }
                }
            }
            section.panel aria-labelledby="overview-gaps" {
                h2 id="overview-gaps" { "Open gaps" }
                @match &data.gaps {
                    Ok(counts) => { (count_table("Resolution", counts, "No gaps are open.", "bbox_gaps")) }
                    Err(error) => { (error_fragment("bbox_gaps", error)) }
                }
            }
        }
    }
}

fn count_table(heading: &str, counts: &[Count], empty: &str, source: &str) -> Markup {
    let total: usize = counts.iter().map(|count| count.value).sum();
    html! {
        @if total == 0 {
            (empty_state(empty, source))
        } @else {
            table {
                thead { tr { th scope="col" { (heading) } th.num scope="col" { "Count" } } }
                tbody {
                    @for count in counts {
                        tr {
                            td {
                                @match count.state {
                                    Some(state) => { span class=(format!("state state-{state}")) { (count.label) } }
                                    None => { (count.label) }
                                }
                            }
                            td.num { (count.value) }
                        }
                    }
                }
            }
            p.panel-source { "Source: " code { (source) } }
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

    async fn overview_body(shared: Arc<SharedState>) -> String {
        let request = Request::builder()
            .uri("/")
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                53000,
            )))
            .header("x-forwarded-email", "operator@example.test")
            .body(Body::empty())
            .unwrap();
        let response = build_ui_router(UiState::new(shared))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn empty_daemon_overview_names_each_empty_region_and_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let body = overview_body(shared).await;

        assert!(body.contains("<h1>Overview</h1>"), "{body}");
        assert!(body.contains("href=\"/\" aria-current=\"page\""));
        assert!(body.contains("No threads are stored."));
        assert!(body.contains("No tasks are on the roster."));
        assert!(body.contains("No projects are registered."));
        assert!(body.contains("No gaps are open."));
        for source in [
            "bbox_thread_list",
            "bro_dashboard",
            "bbox_project_list",
            "bbox_stats",
            "bbox_gaps",
        ] {
            assert!(body.contains(source), "the overview must cite {source}");
        }
        assert!(body.contains("Index documents"), "{body}");
        assert!(!body.contains("error-fragment"), "{body}");
    }

    #[tokio::test]
    async fn overview_counts_threads_and_bros_by_state() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        for topic in ["first", "second"] {
            let open: crate::threads::ThreadParams =
                serde_json::from_value(serde_json::json!({"action": "open", "topic": topic}))
                    .unwrap();
            shared.threads.write().thread(&open).unwrap();
        }
        let provider =
            serde_json::to_value(crate::orchestration::providers::Provider::ALL[0]).unwrap();
        for (task, status) in [("t-1", "running"), ("t-2", "running"), ("t-3", "failed")] {
            let summary: bro_protocol::RosterSummaryV1 = serde_json::from_value(
                serde_json::json!({"task_id": task, "status": status, "provider": provider}),
            )
            .unwrap();
            shared.roster_view.upsert(task.to_string(), summary);
        }
        let body = overview_body(shared).await;

        assert!(
            body.contains(
                "<span class=\"state state-waiting\">open</span></td><td class=\"num\">2</td>"
            ),
            "{body}"
        );
        assert!(body.contains(
            "<span class=\"state state-running\">running</span></td><td class=\"num\">2</td>"
        ));
        assert!(body.contains(
            "<span class=\"state state-failed\">failed</span></td><td class=\"num\">1</td>"
        ));
        assert!(!body.contains("No threads are stored."));
        assert!(!body.contains("No tasks are on the roster."));
    }
}
