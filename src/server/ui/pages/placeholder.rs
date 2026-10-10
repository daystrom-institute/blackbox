//! Sections whose pages are not built yet, and the not-found page. Both
//! render inside the shell so navigation stays complete.

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use maud::html;

use super::super::shell::{Section, layout};
use super::super::{Principal, UiState, html_response};

/// A navigation section that has no page yet.
pub(crate) fn page(ui: &UiState, principal: &Principal, section: Section) -> Response {
    let content = html! {
        p.empty-state { "The " (section.label()) " page is not yet available." }
    };
    html_response(
        StatusCode::OK,
        layout(
            &ui.shell(principal),
            Some(section),
            section.label(),
            content,
        ),
    )
}

/// Any route the UI does not serve.
pub(crate) async fn not_found(
    State(ui): State<Arc<UiState>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let content = html! {
        p.empty-state { "No page lives at this address." }
    };
    html_response(
        StatusCode::NOT_FOUND,
        layout(&ui.shell(&principal), None, "Not found", content),
    )
}
