use std::io;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

pub type FleetdResult<T> = Result<T, FleetdError>;

#[derive(Debug, Error)]
pub enum FleetdError {
    #[error("fleet authority failed: {0}")]
    Authority(#[from] fleet_core::FleetError),
    #[error("fleet RPC failed: {0}")]
    Rpc(#[from] bro_rpc::RpcError),
    #[error("fleet service I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("fleet service serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid fleetd configuration: {0}")]
    InvalidConfiguration(String),
    #[error("fleetd is running in read-only shadow mode")]
    ShadowReadOnly,
    #[error("fleet authority actor is unavailable")]
    AuthorityUnavailable,
    #[error("worker launch failed: {0}")]
    WorkerLaunch(String),
    #[error("capability service unavailable: {0}")]
    CapabilityUnavailable(String),
    #[error("compatibility owner {owner} unavailable: {detail}")]
    CompatibilityUnavailable { owner: &'static str, detail: String },
    #[error("request is invalid: {0}")]
    InvalidRequest(String),
    #[error("entity was not found: {0}")]
    NotFound(String),
    #[error("request conflicts with live fleet state: {0}")]
    Conflict(String),
}

impl FleetdError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidConfiguration(_) | Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::ShadowReadOnly => StatusCode::CONFLICT,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::CapabilityUnavailable(_)
            | Self::CompatibilityUnavailable { .. }
            | Self::AuthorityUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Authority(fleet_core::FleetError::NotFound { .. }) => StatusCode::NOT_FOUND,
            Self::Authority(fleet_core::FleetError::IdempotencyConflict)
            | Self::Authority(fleet_core::FleetError::NoLiveWorker(_))
            | Self::Authority(fleet_core::FleetError::HandshakeRejected(_))
            | Self::Authority(fleet_core::FleetError::WorktreeConflict { .. }) => {
                StatusCode::CONFLICT
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for FleetdError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let code = match &self {
            Self::ShadowReadOnly => "fleet.shadow_read_only",
            Self::AuthorityUnavailable => "fleet.authority_unavailable",
            Self::CapabilityUnavailable(_) => "fleet.capability_unavailable",
            Self::CompatibilityUnavailable { .. } => "fleet.compatibility_owner_unavailable",
            Self::InvalidConfiguration(_) | Self::InvalidRequest(_) => "fleet.invalid_request",
            Self::NotFound(_) | Self::Authority(fleet_core::FleetError::NotFound { .. }) => {
                "fleet.not_found"
            }
            Self::Conflict(_)
            | Self::Authority(fleet_core::FleetError::IdempotencyConflict)
            | Self::Authority(fleet_core::FleetError::NoLiveWorker(_)) => "fleet.conflict",
            _ => "fleet.internal",
        };
        (
            status,
            Json(json!({"error": self.to_string(), "code": code})),
        )
            .into_response()
    }
}
