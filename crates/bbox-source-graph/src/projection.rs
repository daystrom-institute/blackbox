//! Observation-to-graph projection contracts.
//!
//! Observation and projection are separate on purpose. A connector satellite
//! may contact a remote system to build an [`ObservationBatch`], but a
//! [`GraphProjection`] must be deterministic and replayable with no remote
//! access, so the corpus can reproject a retained batch when the schema or the
//! projection logic changes. Durable acceptance belongs to
//! [`crate::SourceProjectionStore`], never to a projection implementation.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use bbox_project_graph::{GraphDescriptor, GraphSchema};

use crate::{GraphDelta, NamedCheckpointTransition, ReconciliationMode, SourceProjectionSnapshot};

/// One observed remote object: stable identity, remote version, observation
/// time, a deletion marker, and an optional typed payload. A deletion carries
/// no payload by construction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    pub observation_id: String,
    /// The source-side entity class this record belongs to (for example
    /// `asset`). Source vocabulary, never a Blackbox enum.
    pub source_entity: String,
    pub remote_id: String,
    pub remote_version: String,
    pub observed_at: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub payload: Option<Value>,
}

/// A bounded set of observations plus the checkpoint transition they justify.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObservationBatch {
    pub batch_id: String,
    pub source_connector: String,
    pub source_scope: String,
    pub reconciliation_mode: ReconciliationMode,
    #[serde(default)]
    pub observations: Vec<ObservationRecord>,
    #[serde(default)]
    pub checkpoint_transition: NamedCheckpointTransition,
}

/// What a projection is allowed to know about where it is projecting into.
/// Deliberately not the whole snapshot: a projection reads prior state through
/// an explicit argument so a replay can be handed a different prior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionContext {
    pub scope_id: String,
    pub graph_id: String,
    pub prior_generation: Option<u64>,
}

impl ProjectionContext {
    /// The generation this projection would produce: exactly one past the
    /// prior.
    pub fn resulting_generation(&self) -> u64 {
        self.prior_generation.unwrap_or(0).saturating_add(1)
    }
}

/// The corpus-plane half of a connector: turn an accepted observation batch
/// into an atomic graph delta.
pub trait GraphProjection: Send + Sync {
    /// The schema this projection version emits. Schema data, not Rust types.
    fn schema_descriptor(&self) -> GraphSchema;

    /// The descriptor for the generation this projection would produce.
    fn graph_descriptor(&self, context: &ProjectionContext) -> GraphDescriptor;

    /// Deterministic for the same batch, schema, projection version, and
    /// prior snapshot. No remote access.
    fn project(
        &self,
        context: &ProjectionContext,
        batch: &ObservationBatch,
        prior: Option<&SourceProjectionSnapshot>,
    ) -> anyhow::Result<GraphDelta>;
}
