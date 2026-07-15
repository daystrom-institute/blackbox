use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use blackops_core::BlackopsError;
use bro_core::BroError;
use serde_json::json;
use thiserror::Error;

pub type BlackopsdResult<T> = Result<T, BlackopsdError>;

#[derive(Debug, Error)]
pub enum BlackopsdError {
    #[error(transparent)]
    Core(#[from] BlackopsError),
    #[error("upstream capability failed ({code}): {message}")]
    Capability { code: String, message: String },
    #[error("HTTP client failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("configuration error: {0}")]
    Configuration(String),
    #[error("operational authority actor is unavailable")]
    AuthorityUnavailable,
    #[error("serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl From<BroError> for BlackopsdError {
    fn from(error: BroError) -> Self {
        Self::Capability {
            code: error.code,
            message: error.message,
        }
    }
}

impl IntoResponse for BlackopsdError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::Core(BlackopsError::NotFound(_)) => (StatusCode::NOT_FOUND, "not_found"),
            Self::Core(BlackopsError::Conflict(_)) => (StatusCode::CONFLICT, "conflict"),
            Self::Core(BlackopsError::InvalidRequest(_)) | Self::InvalidRequest(_) => {
                (StatusCode::BAD_REQUEST, "invalid_request")
            }
            Self::Configuration(_) | Self::AuthorityUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "configuration")
            }
            Self::Capability { .. } | Self::Http(_) => {
                (StatusCode::BAD_GATEWAY, "upstream_unavailable")
            }
            Self::Core(_) | Self::Serialization(_) | Self::Io(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        (
            status,
            Json(json!({
                "error": {
                    "code": code,
                    "message": self.to_string()
                }
            })),
        )
            .into_response()
    }
}
