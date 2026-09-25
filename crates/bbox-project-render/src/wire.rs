//! Checkout-owner render lane between the corpus daemon and a collector.
//!
//! The lane is separate from the producer enroll channel on purpose: a
//! collector that never polls it is never offered a `render_project`
//! command, and a collector that polls a daemon without it keeps enrolling
//! through the unchanged enroll encoding. Polling proves the capability: the
//! request names the transport versions the collector executes and the
//! published scopes whose checkout it currently holds.
//!
//! Routes (all under the authenticated producer channel):
//! - `POST internal/code-source/v1/render-operations/poll`
//! - `GET  internal/code-source/v1/render-operations/{operation_id}/plan?offset=N`
//!   returns one [`crate::transport::ProjectRenderPlanChunkV1`]
//! - `POST internal/code-source/v1/render-operations/{operation_id}/result`

use anyhow::{Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use serde::{Deserialize, Serialize};

use crate::transport::{
    MAX_PROJECT_RENDER_PLAN_BYTES, PROJECT_RENDER_TRANSPORT_VERSION, ProjectRenderReceiptV1,
    validate_render_operation_id,
};

pub const RENDER_LANE_SCHEMA_VERSION: u32 = 1;
pub const RENDER_PROJECT_COMMAND_KIND: &str = "render_project";
pub const MAX_RENDER_LANE_COVERED_SCOPES: usize = 1024;
pub const MAX_RENDER_OPERATIONS_PER_POLL: usize = 8;
pub const MAX_RENDER_LANE_VERSIONS: usize = 8;
pub const MAX_RENDER_LANE_LABEL_BYTES: usize = 256;
pub const MAX_RENDER_LANE_ERROR_BYTES: usize = 4096;
/// Request body bound for the poll route: covered scopes dominate it.
pub const MAX_RENDER_LANE_POLL_BODY_BYTES: usize = 512 * 1024;
/// Request body bound for the result route: one bounded receipt.
pub const MAX_RENDER_LANE_RESULT_BODY_BYTES: usize = 96 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderLanePollRequestV1 {
    pub schema_version: u32,
    /// Project render transport versions this collector can execute.
    pub render_transport_versions: Vec<u32>,
    /// Published scopes whose main-worktree checkout this collector holds.
    pub covered_scopes: Vec<PublishedScope>,
    pub collector_version: String,
}

impl RenderLanePollRequestV1 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RENDER_LANE_SCHEMA_VERSION {
            bail!(
                "unsupported render lane schema version {}",
                self.schema_version
            );
        }
        if self.render_transport_versions.is_empty()
            || self.render_transport_versions.len() > MAX_RENDER_LANE_VERSIONS
        {
            bail!("render lane poll must name between one and eight transport versions");
        }
        if self.covered_scopes.len() > MAX_RENDER_LANE_COVERED_SCOPES {
            bail!("render lane poll covers too many scopes");
        }
        for scope in &self.covered_scopes {
            scope.validate()?;
        }
        validate_label(&self.collector_version, "collector_version")
    }

    pub fn supports_current_transport(&self) -> bool {
        self.render_transport_versions
            .contains(&PROJECT_RENDER_TRANSPORT_VERSION)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderLanePollResponseV1 {
    pub schema_version: u32,
    pub operations: Vec<RenderOperationDeliveryV1>,
}

impl RenderLanePollResponseV1 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RENDER_LANE_SCHEMA_VERSION {
            bail!(
                "unsupported render lane schema version {}",
                self.schema_version
            );
        }
        if self.operations.len() > MAX_RENDER_OPERATIONS_PER_POLL {
            bail!("render lane poll returned too many operations");
        }
        self.operations
            .iter()
            .try_for_each(RenderOperationDeliveryV1::validate)
    }
}

/// One delivered `render_project` command. The plan itself is paged
/// separately and is bound to this operation id, scope, and sequence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderOperationDeliveryV1 {
    pub operation_id: String,
    pub kind: String,
    pub scope: PublishedScope,
    pub sequence: u64,
    pub plan_sha256: String,
    pub plan_bytes: usize,
}

impl RenderOperationDeliveryV1 {
    pub fn validate(&self) -> Result<()> {
        validate_render_operation_id(&self.operation_id)?;
        if self.kind != RENDER_PROJECT_COMMAND_KIND {
            bail!("unsupported render lane command kind");
        }
        self.scope.validate()?;
        if self.sequence == 0 {
            bail!("render operation has no sequence");
        }
        validate_sha256(&self.plan_sha256)?;
        if self.plan_bytes == 0 || self.plan_bytes > MAX_PROJECT_RENDER_PLAN_BYTES {
            bail!("render operation declares an invalid plan length");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderOperationErrorV1 {
    pub code: String,
    pub message: String,
}

impl RenderOperationErrorV1 {
    pub fn validate(&self) -> Result<()> {
        validate_label(&self.code, "error.code")?;
        if self.message.is_empty()
            || self.message.len() > MAX_RENDER_LANE_ERROR_BYTES
            || self.message.bytes().any(|byte| byte == 0)
        {
            bail!("render lane error message is invalid");
        }
        Ok(())
    }
}

/// Terminal result of one delivered operation: an exact receipt, or a typed
/// failure that wrote nothing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderOperationResultRequestV1 {
    pub schema_version: u32,
    pub operation_id: String,
    pub plan_sha256: String,
    /// `applied` (with receipt) or `failed` (with error).
    pub outcome: String,
    pub receipt: Option<ProjectRenderReceiptV1>,
    pub error: Option<RenderOperationErrorV1>,
}

impl RenderOperationResultRequestV1 {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RENDER_LANE_SCHEMA_VERSION {
            bail!(
                "unsupported render lane schema version {}",
                self.schema_version
            );
        }
        validate_render_operation_id(&self.operation_id)?;
        validate_sha256(&self.plan_sha256)?;
        match (self.outcome.as_str(), &self.receipt, &self.error) {
            ("applied", Some(receipt), None) => {
                if receipt.producer.as_ref().map(|p| p.operation_id.as_str())
                    != Some(self.operation_id.as_str())
                {
                    bail!("render result receipt names a different operation");
                }
                Ok(())
            }
            ("failed", None, Some(error)) => error.validate(),
            ("applied" | "failed", _, _) => bail!("render result outcome payload is inconsistent"),
            _ => bail!("render result outcome is invalid"),
        }
    }
}

/// `accepted`: the result is recorded. `already_settled`: an identical result
/// was already recorded. `stale`: recorded, but the daemon's current plan or
/// authority no longer matches, so the result is not current convergence.
/// `superseded`: a newer operation for the project was issued first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenderOperationResultResponseV1 {
    pub status: String,
}

fn validate_label(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_RENDER_LANE_LABEL_BYTES
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        bail!("render lane field {field} is invalid");
    }
    Ok(())
}

pub fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid SHA-256");
    }
    Ok(())
}
