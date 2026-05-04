//! HTTP handlers for the council backend.
//!
//! Mounted from `main.rs`:
//!     POST   /council                  // create
//!     GET    /council                  // list (?project=)
//!     GET    /council/:id              // open / full state snapshot
//!     DELETE /council/:id              // close
//!     POST   /council/:id/post         // user turn
//!     GET    /council/:id/tail         // SSE event stream
//!
//! These are the surface the TUI client (and future operators) speak.
//! MCP read-only tools (`bro_council_list/open/posts`) reuse the same
//! registry methods, defined in `main.rs` next to the other tool
//! handlers.

use std::sync::Arc;

use axum::extract::{Path, Query, State as AxumState};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use super::{
    post_user_turn, CouncilEvent, CouncilPost, CouncilStatus, CouncilSummary, InboxEnvelope,
};

// ── Request / response shapes ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateCouncilRequest {
    pub team: String,
    pub topic: String,
    #[serde(default)]
    pub charter: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateCouncilResponse {
    pub id: String,
    pub topic: String,
    pub team: String,
    pub status: CouncilStatus,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OpenResponse {
    pub summary: CouncilSummary,
    pub posts: Vec<CouncilPost>,
    pub envelopes: Vec<InboxEnvelope>,
    pub charter: String,
}

#[derive(Debug, Deserialize)]
pub struct PostRequest {
    pub body: String,
    #[serde(default = "default_sender")]
    pub sender: String,
}

fn default_sender() -> String {
    "user".to_string()
}

#[derive(Debug, Serialize)]
pub struct PostResponse {
    pub sequence: u64,
}

// ── Handlers ──────────────────────────────────────────────────────────

pub async fn create(
    AxumState(state): AxumState<Arc<crate::SharedState>>,
    Json(req): Json<CreateCouncilRequest>,
) -> impl IntoResponse {
    let store_dir = state.store_dir.clone();
    let res = state
        .councils
        .create(req.team.clone(), req.topic.clone(), req.charter, req.project, &store_dir);
    match res {
        Ok(c) => {
            let s = c.session.read();
            (
                StatusCode::CREATED,
                Json(CreateCouncilResponse {
                    id: s.id.clone(),
                    topic: s.topic.clone(),
                    team: s.team_id.clone(),
                    status: s.status,
                }),
            )
                .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("create failed: {e}")).into_response(),
    }
}

pub async fn list(
    AxumState(state): AxumState<Arc<crate::SharedState>>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    let summaries = state.councils.list_summaries(q.project.as_deref());
    Json(summaries).into_response()
}

pub async fn open(
    AxumState(state): AxumState<Arc<crate::SharedState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(council) = state.councils.get(&id) else {
        return (StatusCode::NOT_FOUND, format!("unknown council: {id}")).into_response();
    };
    let s = council.session.read().clone();
    let posts = council.posts.read().clone();
    let envelopes = council.envelopes.read().clone();
    let summary = CouncilSummary {
        id: s.id.clone(),
        team_id: s.team_id.clone(),
        project: s.project.clone(),
        topic: s.topic.clone(),
        status: s.status,
        members: s.member_sessions.keys().cloned().collect(),
        created_at: s.created_at.clone(),
        updated_at: s.updated_at.clone(),
        post_count: posts.len() as u64,
    };
    Json(OpenResponse {
        summary,
        posts,
        envelopes,
        charter: s.charter,
    })
    .into_response()
}

pub async fn close(
    AxumState(state): AxumState<Arc<crate::SharedState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.councils.close(&id) {
        Ok(()) => (StatusCode::NO_CONTENT, "").into_response(),
        Err(e) => (StatusCode::NOT_FOUND, format!("close failed: {e}")).into_response(),
    }
}

pub async fn post(
    AxumState(state): AxumState<Arc<crate::SharedState>>,
    Path(id): Path<String>,
    Json(req): Json<PostRequest>,
) -> impl IntoResponse {
    let registry = state.councils.clone();
    match post_user_turn(state.clone(), registry, &id, &req.sender, &req.body).await {
        Ok(seq) => (StatusCode::CREATED, Json(PostResponse { sequence: seq })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("post failed: {e}")).into_response(),
    }
}

pub async fn tail(
    AxumState(state): AxumState<Arc<crate::SharedState>>,
    Path(id): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    use async_stream::stream;

    let rx = state.councils.subscribe();
    let target = id.clone();

    let s = stream! {
        if let Some(mut rx) = rx {
            while let Ok(ev) = rx.recv().await {
                let council_id_match = match &ev {
                    CouncilEvent::Post { council_id, .. } => council_id == &target,
                    CouncilEvent::EnvelopeChanged { council_id, .. } => council_id == &target,
                    CouncilEvent::Closed { council_id } => council_id == &target,
                };
                if !council_id_match {
                    continue;
                }
                let payload = serde_json::to_string(&ev).unwrap_or_default();
                let event_name = match &ev {
                    CouncilEvent::Post { .. } => "post",
                    CouncilEvent::EnvelopeChanged { .. } => "envelope",
                    CouncilEvent::Closed { .. } => "closed",
                };
                yield Ok(Event::default().event(event_name).data(payload));
                if matches!(ev, CouncilEvent::Closed { .. }) {
                    return;
                }
            }
        }
    };

    Sse::new(s)
}
