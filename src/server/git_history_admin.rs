//! Operator-only Git-history activation administration.
//!
//! Thin HTTP mirror of the offline `blackbox git-history activations` CLI:
//! the same store reads and drops, exposed on the loopback admin surface so
//! an operator can inspect and clear a dead-lettered activation without
//! shell access to the state dir. Like every other `/admin/*` route this is
//! operator authority, never an MCP tool, and store I/O runs on the blocking
//! pool.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

use super::state::SharedState;
use bbox_corpus_core::project_catalog::RepoHistoryId;

#[derive(Debug, Deserialize)]
pub(crate) struct ActivationsDropParams {
    repo_history: String,
    #[serde(default)]
    retire_ready_pointer: bool,
}

#[derive(Serialize)]
struct ReadyPointerRow {
    repo_history_id: String,
    source_generation_id: String,
    producer_id: String,
    repo_head: String,
}

#[derive(Serialize)]
struct JournalRow {
    repo_history_id: String,
    source_generation_id: String,
    stage: &'static str,
    terminal: bool,
}

#[derive(Serialize)]
struct DeadletterRow {
    repo_history_id: String,
    source_generation_id: String,
    producer_id: String,
    error_code: String,
    attempts: u64,
    first_seen_unix_secs: u64,
    last_seen_unix_secs: u64,
    diagnostic: Option<String>,
}

#[derive(Serialize)]
struct ActivationsView {
    ready_pointers: Vec<ReadyPointerRow>,
    journals: Vec<JournalRow>,
    deadletters: Vec<DeadletterRow>,
}

#[derive(Serialize)]
struct DropView {
    repo_history_id: String,
    dropped: bool,
    retired_ready_pointer: Option<String>,
}

#[derive(Serialize)]
struct AdminError {
    error: String,
}

/// `GET /admin/git-history/activations`: ready pointers, activation
/// journals, and dead letters with error codes and timestamps.
pub(crate) async fn admin_git_history_activations(
    State(state): State<Arc<SharedState>>,
) -> axum::response::Response {
    let store = state.git_sources.store();
    let listed = tokio::task::spawn_blocking(move || {
        let ready_pointers = store
            .current_ready_pointers()?
            .into_iter()
            .map(|pointer| ReadyPointerRow {
                repo_history_id: pointer.repo_history_id.as_str().to_string(),
                source_generation_id: pointer.source_generation_id,
                producer_id: pointer.producer_id,
                repo_head: pointer.repo_head,
            })
            .collect::<Vec<_>>();
        let journals = store
            .list_activation_journals()?
            .into_iter()
            .map(|journal| JournalRow {
                repo_history_id: journal.repo_history_id.as_str().to_string(),
                source_generation_id: journal.source_generation_id,
                stage: stage_label(journal.stage),
                terminal: journal.stage.terminal(),
            })
            .collect::<Vec<_>>();
        let deadletters = store
            .list_activation_deadletters()?
            .into_iter()
            .map(|deadletter| DeadletterRow {
                repo_history_id: deadletter.repo_history_id.as_str().to_string(),
                source_generation_id: deadletter.source_generation_id,
                producer_id: deadletter.producer_id,
                error_code: deadletter.error_code,
                attempts: deadletter.attempts,
                first_seen_unix_secs: deadletter.first_seen_unix_secs,
                last_seen_unix_secs: deadletter.last_seen_unix_secs,
                diagnostic: deadletter.diagnostic,
            })
            .collect::<Vec<_>>();
        Ok::<_, anyhow::Error>(ActivationsView {
            ready_pointers,
            journals,
            deadletters,
        })
    })
    .await;
    match listed {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminError {
                error: format!("git-source store listing failed: {error:#}"),
            }),
        )
            .into_response(),
        Err(join) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminError {
                error: format!("git-history activation listing task failed: {join}"),
            }),
        )
            .into_response(),
    }
}

/// `DELETE /admin/git-history/activations?repo_history=<id>
/// &retire_ready_pointer=true|false`: drop one activation dead letter and,
/// with the flag, retire the repository's orphaned ready pointer. The
/// pointer retires before the letter is dropped so an interrupted call
/// leaves the background redrive stopped rather than resumed.
pub(crate) async fn admin_git_history_activations_drop(
    State(state): State<Arc<SharedState>>,
    axum::extract::Query(params): axum::extract::Query<ActivationsDropParams>,
) -> axum::response::Response {
    let Ok(repo_history) = RepoHistoryId::parse(params.repo_history) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(AdminError {
                error: "repo_history must be a repo history id (rh_ prefixed)".to_string(),
            }),
        )
            .into_response();
    };
    let store = state.git_sources.store();
    let retire = params.retire_ready_pointer;
    let dropped = tokio::task::spawn_blocking(move || {
        let retired = if retire {
            store.retire_current_ready_pointer(&repo_history)?
        } else {
            None
        };
        let dropped = store.drop_activation_deadletter(&repo_history)?;
        Ok::<_, anyhow::Error>(DropView {
            repo_history_id: repo_history.as_str().to_string(),
            dropped,
            retired_ready_pointer: retired,
        })
    })
    .await;
    match dropped {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminError {
                error: format!("git-source store drop failed: {error:#}"),
            }),
        )
            .into_response(),
        Err(join) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminError {
                error: format!("git-history activation drop task failed: {join}"),
            }),
        )
            .into_response(),
    }
}

fn stage_label(stage: bbox_git_source_store::HistoryActivationStageV1) -> &'static str {
    use bbox_git_source_store::HistoryActivationStageV1 as Stage;
    match stage {
        Stage::Prepared => "prepared",
        Stage::GenerationVerified => "generation_verified",
        Stage::MaterializationAdvanced => "materialization_advanced",
        Stage::CommitViewPublished => "commit_view_published",
        Stage::OverlaysPublished => "overlays_published",
        Stage::Committed => "committed",
        Stage::Superseded => "superseded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    fn test_app_with_state() -> (axum::Router, Arc<SharedState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        let app = axum::Router::new()
            .route(
                "/admin/git-history/activations",
                axum::routing::get(admin_git_history_activations)
                    .delete(admin_git_history_activations_drop),
            )
            .with_state(state.clone());
        (app, state, dir)
    }

    #[tokio::test]
    async fn admin_activations_list_and_drop_roundtrip() {
        let (app, state, _dir) = test_app_with_state();
        let store = state.git_sources.store();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000061").unwrap();
        store
            .record_activation_deadletter(
                &history,
                "producer-a",
                "ghs_0000000000000000000000000000000000000000000000000000000000000000",
                "repo_history_not_found",
                Some("no published project binds this repo history".to_string()),
            )
            .unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/admin/git-history/activations")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "listing failed: {}",
            String::from_utf8_lossy(&body)
        );
        let view: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let deadletters = view.get("deadletters").unwrap().as_array().unwrap();
        assert_eq!(deadletters.len(), 1);
        assert_eq!(
            deadletters[0].get("error_code").unwrap().as_str().unwrap(),
            "repo_history_not_found"
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!(
                        "/admin/git-history/activations?repo_history={}",
                        history.as_str()
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let dropped: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dropped.get("dropped").unwrap().as_bool().unwrap(), true);
        assert!(dropped.get("retired_ready_pointer").unwrap().is_null());

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!(
                        "/admin/git-history/activations?repo_history={}",
                        history.as_str()
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let dropped: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(dropped.get("dropped").unwrap().as_bool().unwrap(), false);
    }

    #[tokio::test]
    async fn admin_activations_drop_refuses_a_malformed_repo_history() {
        let (app, _state, _dir) = test_app_with_state();
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/admin/git-history/activations?repo_history=not-an-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
