use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    GraphAuthority, GraphDescriptor, GraphGeneration, GraphKey, GraphSchema, GraphSource,
    ProjectGraphEdge, ProjectGraphVertex, build_generation, validate_graph,
};

pub const SOURCE_PROJECTION_STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NamedCheckpointSet {
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceObservationRef {
    pub observation_id: String,
    pub remote_id: String,
    pub remote_version: String,
    pub observed_at: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationMode {
    Incremental,
    Full,
}

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphDelta {
    pub graph_id: String,
    pub batch_id: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProjectionSnapshot {
    pub store_version: u32,
    pub scope_id: String,
    pub descriptor: GraphDescriptor,
    pub schema: GraphSchema,
    pub vertices: BTreeMap<String, ProjectGraphVertex>,
    pub edges: Vec<ProjectGraphEdge>,
    pub checkpoints: NamedCheckpointSet,
    pub last_batch_id: String,
    pub last_commit_fingerprint: String,
    pub graph_fingerprint: String,
    #[serde(default)]
    pub latest_observed_at: Option<String>,
    pub observations: Vec<SourceObservationRef>,
    pub reconciliation_mode: ReconciliationMode,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceProjectionStatus {
    pub scope_id: String,
    pub graph_id: String,
    pub source_connector: String,
    pub schema_version: u64,
    pub projection_version: String,
    pub generation: u64,
    pub graph_fingerprint: String,
    pub last_batch_id: String,
    pub latest_observed_at: Option<String>,
    pub checkpoints: NamedCheckpointSet,
    pub reconciliation_mode: ReconciliationMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectionCommitFailure {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ProjectionCommitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProjectionCommitFailure {}

#[derive(Debug)]
pub struct SourceProjectionStore {
    path: PathBuf,
    snapshot: Option<SourceProjectionSnapshot>,
}

impl SourceProjectionStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let snapshot = read_snapshot(&path)?;
        Ok(Self { path, snapshot })
    }

    pub fn snapshot(&self) -> Option<&SourceProjectionSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn checkpoints(&self) -> NamedCheckpointSet {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.checkpoints.clone())
            .unwrap_or_default()
    }

    pub fn status(&self) -> Option<SourceProjectionStatus> {
        let snapshot = self.snapshot.as_ref()?;
        Some(SourceProjectionStatus {
            scope_id: snapshot.scope_id.clone(),
            graph_id: snapshot.descriptor.graph_id.clone(),
            source_connector: snapshot
                .descriptor
                .source_connector
                .clone()
                .expect("accepted connector descriptor has source_connector"),
            schema_version: snapshot.schema.version,
            projection_version: snapshot
                .descriptor
                .projection_version
                .clone()
                .expect("accepted connector descriptor has projection_version"),
            generation: snapshot.descriptor.generation,
            graph_fingerprint: snapshot.graph_fingerprint.clone(),
            last_batch_id: snapshot.last_batch_id.clone(),
            latest_observed_at: snapshot.latest_observed_at.clone(),
            checkpoints: snapshot.checkpoints.clone(),
            reconciliation_mode: snapshot.reconciliation_mode,
        })
    }

    pub fn accepted_generation(&self) -> Option<GraphGeneration> {
        self.snapshot
            .as_ref()
            .map(|snapshot| generation_from_snapshot(snapshot, source_root(&self.path)))
    }

    pub fn accept(
        &mut self,
        scope_id: &str,
        descriptor: GraphDescriptor,
        schema: GraphSchema,
        delta: GraphDelta,
    ) -> Result<GraphGeneration> {
        let path = self.path.clone();
        bbox_corpus_core::json_store::with_store_lock(&path, || {
            self.snapshot = read_snapshot(&path)?;
            self.accept_locked(scope_id, descriptor, schema, delta)
        })
    }

    fn accept_locked(
        &mut self,
        scope_id: &str,
        descriptor: GraphDescriptor,
        schema: GraphSchema,
        delta: GraphDelta,
    ) -> Result<GraphGeneration> {
        let commit_fingerprint = fingerprint(&(&descriptor, &schema, &delta))?;
        if let Some(current) = &self.snapshot
            && current.last_batch_id == delta.batch_id
        {
            if current.last_commit_fingerprint == commit_fingerprint {
                return Ok(generation_from_snapshot(current, source_root(&self.path)));
            }
            return projection_failure(
                "projection.batch_conflict",
                format!(
                    "batch `{}` was already accepted with different content",
                    delta.batch_id
                ),
            );
        }

        validate_commit_header(
            scope_id,
            &descriptor,
            &schema,
            &delta,
            self.snapshot.as_ref(),
        )?;
        validate_observation_refs(&delta.observations)?;
        let mut checkpoints = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.checkpoints.clone())
            .unwrap_or_default();
        apply_checkpoint_transition(&mut checkpoints, &delta.checkpoint_transition)?;

        let current_vertices = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.vertices.clone())
            .unwrap_or_default();
        let current_edges = self
            .snapshot
            .as_ref()
            .map(|snapshot| edge_map(&snapshot.edges))
            .unwrap_or_default();
        validate_delta_shape(&current_vertices, &current_edges, &delta)?;

        let mut vertices = current_vertices;
        let mut edges = current_edges;
        for edge in &delta.removed_edges {
            edges.remove(edge);
        }
        for vertex_id in &delta.removed_vertex_ids {
            vertices.remove(vertex_id);
        }
        for vertex in delta
            .inserted_vertices
            .iter()
            .chain(&delta.replaced_vertices)
        {
            vertices.insert(vertex.id.clone(), vertex.clone());
        }
        for edge in &delta.inserted_edges {
            edges.insert(GraphEdgeKey::of(edge), edge.clone());
        }
        let edges = edges.into_values().collect::<Vec<_>>();

        let numbered_vertices = vertices
            .values()
            .cloned()
            .enumerate()
            .map(|(index, vertex)| (index + 1, vertex))
            .collect::<Vec<_>>();
        let numbered_edges = edges
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, edge)| (index + 1, edge))
            .collect::<Vec<_>>();
        let validation_errors = validate_graph(
            &descriptor.graph_id,
            GraphSource::ConnectorManaged,
            &descriptor,
            &schema,
            &numbered_vertices,
            &numbered_edges,
        );
        if !validation_errors.is_empty() {
            return projection_failure(
                "projection.graph_invalid",
                format!(
                    "projected generation failed graph validation: {}",
                    serde_json::to_string(&validation_errors)?
                ),
            );
        }

        let graph_fingerprint = fingerprint(&(&descriptor, &schema, &vertices, &edges))?;
        let latest_observed_at = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.latest_observed_at.clone())
            .into_iter()
            .chain(
                delta
                    .observations
                    .iter()
                    .map(|observation| observation.observed_at.clone()),
            )
            .max();
        let snapshot = SourceProjectionSnapshot {
            store_version: SOURCE_PROJECTION_STORE_VERSION,
            scope_id: scope_id.to_string(),
            descriptor,
            schema,
            vertices,
            edges,
            checkpoints,
            last_batch_id: delta.batch_id,
            last_commit_fingerprint: commit_fingerprint,
            graph_fingerprint,
            latest_observed_at,
            observations: delta.observations,
            reconciliation_mode: delta.reconciliation_mode,
        };
        bbox_corpus_core::json_store::atomic_write_json_locked(&self.path, &snapshot)
            .with_context(|| format!("writing source projection {}", self.path.display()))?;
        let generation = generation_from_snapshot(&snapshot, source_root(&self.path));
        self.snapshot = Some(snapshot);
        Ok(generation)
    }
}

fn validate_commit_header(
    scope_id: &str,
    descriptor: &GraphDescriptor,
    schema: &GraphSchema,
    delta: &GraphDelta,
    current: Option<&SourceProjectionSnapshot>,
) -> Result<()> {
    if scope_id.trim().is_empty() {
        return projection_failure("projection.empty_scope", "scope_id must not be empty");
    }
    if descriptor.authority != GraphAuthority::Connector {
        return projection_failure(
            "projection.authority_required",
            "source projections require connector authority",
        );
    }
    if descriptor.graph_id != delta.graph_id {
        return projection_failure(
            "projection.graph_id_mismatch",
            format!(
                "descriptor graph_id `{}` does not match delta graph_id `{}`",
                descriptor.graph_id, delta.graph_id
            ),
        );
    }
    if descriptor.generation != delta.resulting_generation {
        return projection_failure(
            "projection.generation_mismatch",
            "descriptor generation must equal delta resulting_generation",
        );
    }
    if descriptor.projection_version.as_deref() != Some(delta.projection_version.as_str()) {
        return projection_failure(
            "projection.version_mismatch",
            "descriptor projection_version must equal delta projection_version",
        );
    }
    if descriptor.schema_version != schema.version {
        return projection_failure(
            "projection.schema_version_mismatch",
            "descriptor schema_version must equal schema version",
        );
    }
    if delta.batch_id.trim().is_empty() {
        return projection_failure("projection.empty_batch_id", "batch_id must not be empty");
    }
    if delta.projection_version.trim().is_empty() {
        return projection_failure(
            "projection.empty_version",
            "projection_version must not be empty",
        );
    }

    let expected_prior = current.map(|snapshot| snapshot.descriptor.generation);
    if delta.prior_generation != expected_prior {
        return projection_failure(
            "projection.prior_generation_conflict",
            format!(
                "delta prior_generation {:?} does not match accepted generation {:?}",
                delta.prior_generation, expected_prior
            ),
        );
    }
    let expected_generation = expected_prior.unwrap_or(0).saturating_add(1);
    if delta.resulting_generation != expected_generation {
        return projection_failure(
            "projection.non_monotonic_generation",
            format!(
                "resulting_generation {} must be exactly {}",
                delta.resulting_generation, expected_generation
            ),
        );
    }

    if let Some(current) = current {
        if current.scope_id != scope_id || current.descriptor.graph_id != descriptor.graph_id {
            return projection_failure(
                "projection.identity_conflict",
                "projection store scope and graph identity cannot change",
            );
        }
        if current.descriptor.schema_id != descriptor.schema_id {
            return projection_failure(
                "projection.schema_identity_conflict",
                "schema_id cannot change within a source graph",
            );
        }
        if schema.version < current.schema.version {
            return projection_failure(
                "projection.schema_rollback",
                "schema version cannot move backwards",
            );
        }
        if current.descriptor.source_connector != descriptor.source_connector {
            return projection_failure(
                "projection.connector_conflict",
                "source_connector cannot change within a source graph",
            );
        }
    }
    Ok(())
}

fn apply_checkpoint_transition(
    checkpoints: &mut NamedCheckpointSet,
    transition: &NamedCheckpointTransition,
) -> Result<()> {
    for (name, advance) in &transition.advances {
        if !valid_checkpoint_name(name) {
            return projection_failure(
                "checkpoint.invalid_name",
                format!("checkpoint name `{name}` is not a safe identifier"),
            );
        }
        if advance.after.trim().is_empty() {
            return projection_failure(
                "checkpoint.empty_value",
                format!("checkpoint `{name}` has an empty next value"),
            );
        }
        let current = checkpoints.values.get(name);
        if current != advance.before.as_ref() {
            return projection_failure(
                "checkpoint.conflict",
                format!(
                    "checkpoint `{name}` expected {:?}, accepted value is {:?}",
                    advance.before, current
                ),
            );
        }
        if advance.before.as_deref() == Some(advance.after.as_str()) {
            return projection_failure(
                "checkpoint.no_advance",
                format!("checkpoint `{name}` did not advance"),
            );
        }
    }
    for (name, advance) in &transition.advances {
        checkpoints
            .values
            .insert(name.clone(), advance.after.clone());
    }
    Ok(())
}

fn validate_delta_shape(
    current_vertices: &BTreeMap<String, ProjectGraphVertex>,
    current_edges: &BTreeMap<GraphEdgeKey, ProjectGraphEdge>,
    delta: &GraphDelta,
) -> Result<()> {
    let inserted_vertex_ids = unique_vertex_ids(
        "projection.duplicate_inserted_vertex",
        &delta.inserted_vertices,
    )?;
    let replaced_vertex_ids = unique_vertex_ids(
        "projection.duplicate_replaced_vertex",
        &delta.replaced_vertices,
    )?;
    let removed_vertex_ids = unique_strings(
        "projection.duplicate_removed_vertex",
        &delta.removed_vertex_ids,
    )?;
    if !inserted_vertex_ids.is_disjoint(&replaced_vertex_ids)
        || !inserted_vertex_ids.is_disjoint(&removed_vertex_ids)
        || !replaced_vertex_ids.is_disjoint(&removed_vertex_ids)
    {
        return projection_failure(
            "projection.vertex_operation_conflict",
            "one vertex id cannot appear in multiple delta operation classes",
        );
    }
    for id in &inserted_vertex_ids {
        if current_vertices.contains_key(id) {
            return projection_failure(
                "projection.insert_existing_vertex",
                format!("inserted vertex `{id}` already exists"),
            );
        }
    }
    for id in &replaced_vertex_ids {
        if !current_vertices.contains_key(id) {
            return projection_failure(
                "projection.replace_missing_vertex",
                format!("replaced vertex `{id}` does not exist"),
            );
        }
    }
    for id in &removed_vertex_ids {
        if !current_vertices.contains_key(id) {
            return projection_failure(
                "projection.remove_missing_vertex",
                format!("removed vertex `{id}` does not exist"),
            );
        }
    }

    let inserted_edge_keys = unique_edge_keys(
        "projection.duplicate_inserted_edge",
        delta.inserted_edges.iter().map(GraphEdgeKey::of),
    )?;
    let removed_edge_keys = unique_edge_keys(
        "projection.duplicate_removed_edge",
        delta.removed_edges.iter().cloned(),
    )?;
    for key in &removed_edge_keys {
        if !current_edges.contains_key(key) {
            return projection_failure(
                "projection.remove_missing_edge",
                format!("removed edge {} does not exist", edge_key_label(key)),
            );
        }
    }
    for key in &inserted_edge_keys {
        if current_edges.contains_key(key) && !removed_edge_keys.contains(key) {
            return projection_failure(
                "projection.insert_existing_edge",
                format!(
                    "inserted edge {} already exists and was not removed first",
                    edge_key_label(key)
                ),
            );
        }
    }
    Ok(())
}

fn validate_observation_refs(observations: &[SourceObservationRef]) -> Result<()> {
    let mut observation_ids = BTreeSet::new();
    for observation in observations {
        if observation.observation_id.trim().is_empty()
            || observation.remote_id.trim().is_empty()
            || observation.remote_version.trim().is_empty()
            || observation.observed_at.trim().is_empty()
        {
            return projection_failure(
                "projection.invalid_observation_ref",
                "observation references require non-empty observation_id, remote_id, remote_version, and observed_at",
            );
        }
        if !observation_ids.insert(&observation.observation_id) {
            return projection_failure(
                "projection.duplicate_observation_ref",
                format!(
                    "observation_id `{}` appears more than once",
                    observation.observation_id
                ),
            );
        }
    }
    Ok(())
}

fn unique_vertex_ids(code: &str, vertices: &[ProjectGraphVertex]) -> Result<BTreeSet<String>> {
    unique_strings(
        code,
        &vertices
            .iter()
            .map(|vertex| vertex.id.clone())
            .collect::<Vec<_>>(),
    )
}

fn unique_strings(code: &str, values: &[String]) -> Result<BTreeSet<String>> {
    let set = values.iter().cloned().collect::<BTreeSet<_>>();
    if set.len() != values.len() {
        return projection_failure(code, "delta contains duplicate identifiers");
    }
    Ok(set)
}

fn unique_edge_keys(
    code: &str,
    values: impl IntoIterator<Item = GraphEdgeKey>,
) -> Result<BTreeSet<GraphEdgeKey>> {
    let values = values.into_iter().collect::<Vec<_>>();
    let set = values.iter().cloned().collect::<BTreeSet<_>>();
    if set.len() != values.len() {
        return projection_failure(code, "delta contains duplicate edge identities");
    }
    Ok(set)
}

fn edge_map(edges: &[ProjectGraphEdge]) -> BTreeMap<GraphEdgeKey, ProjectGraphEdge> {
    edges
        .iter()
        .cloned()
        .map(|edge| (GraphEdgeKey::of(&edge), edge))
        .collect()
}

fn edge_key_label(key: &GraphEdgeKey) -> String {
    format!("(`{}`, `{}`, `{}`)", key.from, key.type_name, key.to)
}

fn valid_checkpoint_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn generation_from_snapshot(
    snapshot: &SourceProjectionSnapshot,
    source_root: PathBuf,
) -> GraphGeneration {
    build_generation(
        GraphKey {
            scope_id: snapshot.scope_id.clone(),
            graph_id: snapshot.descriptor.graph_id.clone(),
            source: GraphSource::ConnectorManaged,
        },
        snapshot.descriptor.clone(),
        snapshot.schema.clone(),
        snapshot.vertices.values().cloned().collect(),
        snapshot.edges.clone(),
        snapshot.graph_fingerprint.clone(),
        source_root,
    )
}

fn source_root(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_snapshot(path: &Path) -> Result<Option<SourceProjectionSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let snapshot: SourceProjectionSnapshot =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if snapshot.store_version != SOURCE_PROJECTION_STORE_VERSION {
        return projection_failure(
            "projection.unsupported_store_version",
            format!(
                "projection store version {} is unsupported; expected {}",
                snapshot.store_version, SOURCE_PROJECTION_STORE_VERSION
            ),
        );
    }
    validate_snapshot(&snapshot)?;
    Ok(Some(snapshot))
}

fn validate_snapshot(snapshot: &SourceProjectionSnapshot) -> Result<()> {
    if snapshot.scope_id.trim().is_empty()
        || snapshot.last_batch_id.trim().is_empty()
        || !valid_fingerprint(&snapshot.last_commit_fingerprint)
    {
        return projection_failure(
            "projection.invalid_snapshot_metadata",
            "stored projection has invalid scope, batch, or commit metadata",
        );
    }
    validate_observation_refs(&snapshot.observations)?;
    for (name, value) in &snapshot.checkpoints.values {
        if !valid_checkpoint_name(name) || value.trim().is_empty() {
            return projection_failure(
                "projection.invalid_snapshot_checkpoint",
                format!("stored checkpoint `{name}` is invalid"),
            );
        }
    }
    let numbered_vertices = snapshot
        .vertices
        .values()
        .cloned()
        .enumerate()
        .map(|(index, vertex)| (index + 1, vertex))
        .collect::<Vec<_>>();
    let numbered_edges = snapshot
        .edges
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, edge)| (index + 1, edge))
        .collect::<Vec<_>>();
    let errors = validate_graph(
        &snapshot.descriptor.graph_id,
        GraphSource::ConnectorManaged,
        &snapshot.descriptor,
        &snapshot.schema,
        &numbered_vertices,
        &numbered_edges,
    );
    if !errors.is_empty() {
        return projection_failure(
            "projection.invalid_snapshot_graph",
            format!(
                "stored projection failed graph validation: {}",
                serde_json::to_string(&errors)?
            ),
        );
    }
    let graph_fingerprint = fingerprint(&(
        &snapshot.descriptor,
        &snapshot.schema,
        &snapshot.vertices,
        &snapshot.edges,
    ))?;
    if graph_fingerprint != snapshot.graph_fingerprint {
        return projection_failure(
            "projection.snapshot_fingerprint_mismatch",
            "stored projection graph fingerprint does not match its content",
        );
    }
    Ok(())
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fingerprint(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn projection_failure<T>(code: impl Into<String>, message: impl Into<String>) -> Result<T> {
    Err(ProjectionCommitFailure {
        code: code.into(),
        message: message.into(),
    }
    .into())
}
