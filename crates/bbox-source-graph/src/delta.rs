use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use bbox_project_graph::{ProjectGraphEdge, ProjectGraphVertex};

/// A typed acceptance failure. Every refusal carries a stable `code` so
/// callers and tests can assert on the contract clause rather than on prose.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectionFailure {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ProjectionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProjectionFailure {}

/// The stable failure code carried by an acceptance error, when it has one.
/// IO and serialization errors have none.
pub fn projection_failure_code(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<ProjectionFailure>()
        .map(|failure| failure.code.as_str())
}

pub(crate) fn projection_failure<T>(
    code: impl Into<String>,
    message: impl Into<String>,
) -> anyhow::Result<T> {
    Err(ProjectionFailure {
        code: code.into(),
        message: message.into(),
    }
    .into())
}

/// The accepted value of every named checkpoint for one source graph.
///
/// Checkpoints are a NAMED SET rather than one opaque cursor because a real
/// API dataset refreshes its entities, files, associations, and reports on
/// different cadences with different delta support.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamedCheckpointSet {
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

impl NamedCheckpointSet {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

/// One checkpoint's compare-and-set advance. `before` is the value the
/// producer believed was accepted; a mismatch is a conflict, not a
/// last-writer-wins overwrite.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointAdvance {
    #[serde(default)]
    pub before: Option<String>,
    pub after: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamedCheckpointTransition {
    #[serde(default)]
    pub advances: BTreeMap<String, CheckpointAdvance>,
}

/// A reference back to the accepted observation that produced part of a
/// delta. References only: payloads live in the retained observation blob, not
/// in graph facts and never in status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceObservationRef {
    pub observation_id: String,
    pub remote_id: String,
    pub remote_version: String,
    pub observed_at: String,
}

/// Whether a batch observed a bounded change set or the complete source
/// scope. Full reconciliation is the only mode in which a projection may
/// conclude that an unmentioned vertex is gone.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationMode {
    Incremental,
    Full,
}

/// The identity of one edge inside a source graph. Edges have no id of their
/// own, so removal names the triple.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct GraphEdgeKey {
    pub from: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub to: String,
}

impl GraphEdgeKey {
    pub fn of(edge: &ProjectGraphEdge) -> Self {
        Self {
            from: edge.from.clone(),
            type_name: edge.type_name.clone(),
            to: edge.to.clone(),
        }
    }

    pub(crate) fn label(&self) -> String {
        format!("(`{}`, `{}`, `{}`)", self.from, self.type_name, self.to)
    }
}

/// One atomic projection result: the graph change, the observations that
/// justify it, and the checkpoint transition that goes with it.
///
/// Ordering inside the vectors is part of the value: a projection must emit
/// deterministic order for the same accepted batch, schema, and projection
/// version, because the commit fingerprint that makes replay idempotent is
/// taken over the serialized delta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphDelta {
    pub graph_id: String,
    pub batch_id: String,
    /// The generation this delta was computed against. `None` means "no
    /// accepted generation yet".
    #[serde(default)]
    pub prior_generation: Option<u64>,
    pub resulting_generation: u64,
    pub projection_version: String,
    pub reconciliation_mode: ReconciliationMode,
    #[serde(default)]
    pub inserted_vertices: Vec<ProjectGraphVertex>,
    #[serde(default)]
    pub replaced_vertices: Vec<ProjectGraphVertex>,
    #[serde(default)]
    pub removed_vertex_ids: Vec<String>,
    #[serde(default)]
    pub inserted_edges: Vec<ProjectGraphEdge>,
    #[serde(default)]
    pub removed_edges: Vec<GraphEdgeKey>,
    #[serde(default)]
    pub observations: Vec<SourceObservationRef>,
    #[serde(default)]
    pub checkpoint_transition: NamedCheckpointTransition,
    /// Explicit permission for a full reconciliation that observed nothing to
    /// remove everything. Without it, "the remote returned nothing" and "the
    /// remote is empty" are not allowed to look the same.
    #[serde(default)]
    pub allow_empty_full_reconciliation: bool,
}

impl GraphDelta {
    /// A delta that changes no graph facts. Useful for a checkpoint-only
    /// advance, which still costs a generation because the checkpoint set and
    /// the graph are accepted together.
    pub fn empty(
        graph_id: impl Into<String>,
        batch_id: impl Into<String>,
        prior_generation: Option<u64>,
        projection_version: impl Into<String>,
        reconciliation_mode: ReconciliationMode,
    ) -> Self {
        Self {
            graph_id: graph_id.into(),
            batch_id: batch_id.into(),
            prior_generation,
            resulting_generation: prior_generation.unwrap_or(0).saturating_add(1),
            projection_version: projection_version.into(),
            reconciliation_mode,
            inserted_vertices: Vec::new(),
            replaced_vertices: Vec::new(),
            removed_vertex_ids: Vec::new(),
            inserted_edges: Vec::new(),
            removed_edges: Vec::new(),
            observations: Vec::new(),
            checkpoint_transition: NamedCheckpointTransition::default(),
            allow_empty_full_reconciliation: false,
        }
    }

    pub fn touches_facts(&self) -> bool {
        !self.inserted_vertices.is_empty()
            || !self.replaced_vertices.is_empty()
            || !self.removed_vertex_ids.is_empty()
            || !self.inserted_edges.is_empty()
            || !self.removed_edges.is_empty()
    }
}

pub(crate) fn valid_checkpoint_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}
