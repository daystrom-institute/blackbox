use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bbox_corpus_core::json_store::{atomic_write_bytes_locked, with_store_lock};
use bbox_project_graph::{
    GraphAuthority, GraphDescriptor, GraphGeneration, GraphKey, GraphSchema, GraphSource,
    ProjectGraphEdge, ProjectGraphVertex, build_generation, validate_graph,
};

use crate::delta::{projection_failure, valid_checkpoint_name};
use crate::observations::{list_retained, read_retained};
use crate::{
    GraphDelta, GraphEdgeKey, NamedCheckpointSet, NamedCheckpointTransition, ObservationBatch,
    ObservationRetentionPolicy, ObservationSweepStats, ReconciliationMode, ReplayPlan,
    RetainedObservationBatch, RetainedObservationRef, SourceObservationRef, SourceProjectionPaths,
    is_sha256_hex, observation_digest,
};

pub const SOURCE_PROJECTION_STORE_VERSION: u32 = 1;

/// The accepted generation of one connector-managed source graph: descriptor,
/// schema, normalized facts, observation references, and the named checkpoint
/// set, as ONE value in ONE file. The single-file shape is what makes an
/// acceptance atomic without a manifest to reconcile against.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Fingerprint of the exact (descriptor, schema, delta) that produced this
    /// generation. Replay is idempotent only against this value.
    pub last_commit_fingerprint: String,
    pub graph_fingerprint: String,
    #[serde(default)]
    pub latest_observed_at: Option<String>,
    pub observations: Vec<SourceObservationRef>,
    pub reconciliation_mode: ReconciliationMode,
    /// Content address of the retained observation batch behind this
    /// generation, when the caller retained one.
    #[serde(default)]
    pub retained_observation_digest: Option<String>,
    pub accepted_at_unix: u64,
}

/// Operator-visible freshness and identity. Carries no payload, no observation
/// body, and no credential material of any kind.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceProjectionStatus {
    pub scope_id: String,
    pub graph_id: String,
    pub source_connector: String,
    pub schema_id: String,
    pub schema_version: u64,
    pub projection_version: String,
    pub generation: u64,
    pub graph_fingerprint: String,
    pub last_batch_id: String,
    pub latest_observed_at: Option<String>,
    pub reconciliation_mode: ReconciliationMode,
    pub checkpoints: NamedCheckpointSet,
    pub retained_observation_count: usize,
    pub prior_generation_available: bool,
}

/// The dedicated store for connector-managed source graph generations.
///
/// See the crate doc for the layout, the write ordering, and the crash
/// windows. One instance addresses exactly one `(scope_id, graph_id)`.
#[derive(Debug)]
pub struct SourceProjectionStore {
    paths: SourceProjectionPaths,
    scope_id: String,
    graph_id: String,
    snapshot: Option<SourceProjectionSnapshot>,
    retention: ObservationRetentionPolicy,
}

impl SourceProjectionStore {
    pub fn open(root: impl Into<PathBuf>, scope_id: &str, graph_id: &str) -> Result<Self> {
        Self::open_with_retention(
            root,
            scope_id,
            graph_id,
            ObservationRetentionPolicy::default(),
        )
    }

    pub fn open_with_retention(
        root: impl Into<PathBuf>,
        scope_id: &str,
        graph_id: &str,
        retention: ObservationRetentionPolicy,
    ) -> Result<Self> {
        let paths = SourceProjectionPaths::new(root);
        let snapshot_path = paths.snapshot(scope_id, graph_id)?;
        let snapshot = read_snapshot(&snapshot_path)?;
        if let Some(snapshot) = &snapshot
            && (snapshot.scope_id != scope_id || snapshot.descriptor.graph_id != graph_id)
        {
            return projection_failure(
                "projection.identity_conflict",
                "stored projection does not belong to the requested scope and graph",
            );
        }
        Ok(Self {
            paths,
            scope_id: scope_id.to_string(),
            graph_id: graph_id.to_string(),
            snapshot,
            retention,
        })
    }

    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    pub fn snapshot(&self) -> Option<&SourceProjectionSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn accepted_generation_number(&self) -> Option<u64> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.descriptor.generation)
    }

    pub fn checkpoints(&self) -> NamedCheckpointSet {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.checkpoints.clone())
            .unwrap_or_default()
    }

    /// The reflected accepted generation, identical in shape to what the
    /// checkout loader produces for a project-authored graph.
    pub fn accepted_generation(&self) -> Option<GraphGeneration> {
        self.snapshot
            .as_ref()
            .map(|snapshot| generation_from_snapshot(snapshot, self.graph_dir()))
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
                .unwrap_or_default(),
            schema_id: snapshot.descriptor.schema_id.clone(),
            schema_version: snapshot.schema.version,
            projection_version: snapshot
                .descriptor
                .projection_version
                .clone()
                .unwrap_or_default(),
            generation: snapshot.descriptor.generation,
            graph_fingerprint: snapshot.graph_fingerprint.clone(),
            last_batch_id: snapshot.last_batch_id.clone(),
            latest_observed_at: snapshot.latest_observed_at.clone(),
            reconciliation_mode: snapshot.reconciliation_mode,
            checkpoints: snapshot.checkpoints.clone(),
            retained_observation_count: self
                .retained_observations()
                .map(|retained| retained.len())
                .unwrap_or(0),
            prior_generation_available: self
                .prior_snapshot()
                .map(|prior| prior.is_some())
                .unwrap_or(false),
        })
    }

    /// The retained prior generation, for diagnosis only.
    ///
    /// A prior copy whose generation is not exactly one below the accepted
    /// generation is the crash-window duplicate described in the crate doc:
    /// it is reported as unavailable rather than served as history.
    pub fn prior_snapshot(&self) -> Result<Option<SourceProjectionSnapshot>> {
        let path = self.paths.prior_snapshot(&self.scope_id, &self.graph_id)?;
        let Some(prior) = read_snapshot(&path)? else {
            return Ok(None);
        };
        let Some(accepted) = self.accepted_generation_number() else {
            return Ok(None);
        };
        Ok((prior.descriptor.generation + 1 == accepted).then_some(prior))
    }

    pub fn retained_observations(&self) -> Result<Vec<RetainedObservationRef>> {
        Ok(self
            .retained_batches()?
            .into_iter()
            .map(|(_, retained)| retained.reference())
            .collect())
    }

    pub fn load_retained_batch(&self, digest: &str) -> Result<ObservationBatch> {
        let path = self
            .paths
            .observation_blob(&self.scope_id, &self.graph_id, digest)?;
        if !path.exists() {
            return projection_failure(
                "observations.not_retained",
                format!("observation batch `{digest}` is no longer retained"),
            );
        }
        Ok(read_retained(&path)?.batch)
    }

    /// The ordered replay of retained batches past `from_generation`.
    ///
    /// `complete == false` means a generation inside the requested range has
    /// fallen outside the retention horizon. The honest response is to
    /// re-observe, not to project the partial history that remains.
    pub fn replay_plan(&self, from_generation: u64) -> Result<ReplayPlan> {
        let retained = self.retained_observations()?;
        let accepted = self.accepted_generation_number().unwrap_or(0);
        let available = retained
            .iter()
            .map(|item| item.generation)
            .collect::<BTreeSet<_>>();
        let needed = (from_generation.saturating_add(1)..=accepted).collect::<BTreeSet<_>>();
        Ok(ReplayPlan {
            batches: retained
                .into_iter()
                .filter(|item| item.generation > from_generation)
                .collect(),
            earliest_retained_generation: available.iter().next().copied(),
            complete: needed.is_subset(&available),
        })
    }

    /// Reclaim retained batches outside both retention windows. Advisory
    /// state only: the accepted generation is never touched.
    pub fn sweep_retained_observations(&self, now_unix: u64) -> Result<ObservationSweepStats> {
        let accepted = self.accepted_generation_number().unwrap_or(0);
        let snapshot_path = self.paths.snapshot(&self.scope_id, &self.graph_id)?;
        let retained = self.retained_batches()?;
        let policy = self.retention;
        with_store_lock(&snapshot_path, || {
            let mut stats = ObservationSweepStats::default();
            for (path, batch) in &retained {
                stats.examined += 1;
                if policy.retains(batch, accepted, now_unix) {
                    continue;
                }
                fs::remove_file(path).with_context(|| format!("reclaiming {}", path.display()))?;
                stats.reclaimed += 1;
            }
            Ok(stats)
        })
    }

    /// Accept one atomic snapshot: descriptor, schema, projected facts,
    /// observation references, and the named checkpoint transition together.
    ///
    /// `batch` is the observation batch to retain content addressed for later
    /// reprojection. Pass `None` when the caller replays from deterministic
    /// fixtures instead of retained observations.
    pub fn accept(
        &mut self,
        descriptor: GraphDescriptor,
        schema: GraphSchema,
        delta: GraphDelta,
        batch: Option<&ObservationBatch>,
    ) -> Result<GraphGeneration> {
        let graph_dir = self.paths.graph_dir(&self.scope_id, &self.graph_id)?;
        fs::create_dir_all(&graph_dir)
            .with_context(|| format!("creating {}", graph_dir.display()))?;
        let snapshot_path = self.paths.snapshot(&self.scope_id, &self.graph_id)?;
        let lock_path = snapshot_path.clone();
        with_store_lock(&lock_path, || {
            // The file is the authority, so re-read it under the lock rather
            // than trusting the in-memory copy.
            self.snapshot = read_snapshot(&snapshot_path)?;
            self.accept_locked(descriptor, schema, delta, batch)
        })
    }

    fn accept_locked(
        &mut self,
        descriptor: GraphDescriptor,
        schema: GraphSchema,
        delta: GraphDelta,
        batch: Option<&ObservationBatch>,
    ) -> Result<GraphGeneration> {
        let commit_fingerprint = fingerprint(&(&descriptor, &schema, &delta))?;
        if let Some(current) = &self.snapshot
            && current.last_batch_id == delta.batch_id
        {
            // Only an EXACT replay of the most recently accepted batch is
            // idempotent. Same batch id with different content is a conflict.
            if current.last_commit_fingerprint == commit_fingerprint {
                return Ok(generation_from_snapshot(current, self.graph_dir()));
            }
            return projection_failure(
                "projection.batch_conflict",
                format!(
                    "batch `{}` was already accepted with different content",
                    delta.batch_id
                ),
            );
        }

        self.validate_commit_header(&descriptor, &schema, &delta)?;
        validate_observation_refs(&delta.observations)?;
        if let Some(batch) = batch
            && batch.batch_id != delta.batch_id
        {
            return projection_failure(
                "projection.batch_identity_mismatch",
                format!(
                    "retained batch `{}` does not match delta batch `{}`",
                    batch.batch_id, delta.batch_id
                ),
            );
        }

        let mut checkpoints = self.checkpoints();
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

        // Removing a vertex does NOT cascade to its edges. A delta that
        // strands an edge is refused by name rather than surfacing as a
        // generic graph validation failure, because the connector author
        // needs to know exactly which edge they forgot to remove.
        let dangling = edges
            .keys()
            .filter(|key| !vertices.contains_key(&key.from) || !vertices.contains_key(&key.to))
            .map(GraphEdgeKey::label)
            .collect::<Vec<_>>();
        if !dangling.is_empty() {
            return projection_failure(
                "projection.dangling_edge_after_removal",
                format!(
                    "delta leaves edges without endpoints: {}",
                    dangling.join(", ")
                ),
            );
        }
        let edges = edges.into_values().collect::<Vec<_>>();

        let validation_errors = validate_graph(
            &descriptor.graph_id,
            GraphSource::ConnectorManaged,
            &descriptor,
            &schema,
            &numbered(vertices.values().cloned()),
            &numbered(edges.iter().cloned()),
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
        let accepted_at_unix = now_unix();
        let generation_number = delta.resulting_generation;

        // WRITE ORDER, and the only place it may change is the crate doc.
        // 1. retain the observation batch (content addressed, immutable)
        let retained_observation_digest = match batch {
            Some(batch) => Some(self.retain_batch(batch, generation_number, accepted_at_unix)?),
            None => None,
        };
        // 2. copy the currently accepted snapshot aside for diagnosis
        if let Some(current) = &self.snapshot {
            let prior_path = self.paths.prior_snapshot(&self.scope_id, &self.graph_id)?;
            atomic_write_bytes_locked(&prior_path, &serde_json::to_vec(current)?)
                .with_context(|| format!("writing {}", prior_path.display()))?;
        }

        let snapshot = SourceProjectionSnapshot {
            store_version: SOURCE_PROJECTION_STORE_VERSION,
            scope_id: self.scope_id.clone(),
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
            retained_observation_digest,
            accepted_at_unix,
        };
        // 3. THE COMMIT POINT
        let snapshot_path = self.paths.snapshot(&self.scope_id, &self.graph_id)?;
        atomic_write_bytes_locked(&snapshot_path, &serde_json::to_vec(&snapshot)?)
            .with_context(|| format!("writing {}", snapshot_path.display()))?;
        let generation = generation_from_snapshot(&snapshot, self.graph_dir());
        self.snapshot = Some(snapshot);
        Ok(generation)
    }

    fn validate_commit_header(
        &self,
        descriptor: &GraphDescriptor,
        schema: &GraphSchema,
        delta: &GraphDelta,
    ) -> Result<()> {
        if descriptor.authority != GraphAuthority::Connector {
            return projection_failure(
                "projection.authority_required",
                "the source projection store accepts connector authority only; \
                 a connector refresh cannot write a project-authored graph",
            );
        }
        if descriptor.graph_id != self.graph_id || delta.graph_id != self.graph_id {
            return projection_failure(
                "projection.graph_id_mismatch",
                format!(
                    "descriptor graph_id `{}` and delta graph_id `{}` must both be `{}`",
                    descriptor.graph_id, delta.graph_id, self.graph_id
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
        if delta.reconciliation_mode == ReconciliationMode::Full
            && !delta.removed_vertex_ids.is_empty()
            && delta.observations.is_empty()
            && !delta.allow_empty_full_reconciliation
        {
            return projection_failure(
                "projection.empty_full_reconciliation",
                "a full reconciliation that observed nothing cannot remove everything; \
                 set allow_empty_full_reconciliation to assert the source really is empty",
            );
        }

        let expected_prior = self.accepted_generation_number();
        if delta.prior_generation != expected_prior {
            return projection_failure(
                "projection.prior_generation_conflict",
                format!(
                    "delta prior_generation {:?} does not match accepted generation {expected_prior:?}",
                    delta.prior_generation
                ),
            );
        }
        let expected_generation = expected_prior.unwrap_or(0).saturating_add(1);
        if delta.resulting_generation != expected_generation {
            return projection_failure(
                "projection.non_monotonic_generation",
                format!(
                    "resulting_generation {} must be exactly {expected_generation}: \
                     generations advance by one",
                    delta.resulting_generation
                ),
            );
        }

        if let Some(current) = &self.snapshot {
            if current.descriptor.schema_id != descriptor.schema_id {
                return projection_failure(
                    "projection.schema_identity_conflict",
                    "schema_id cannot change within a source graph",
                );
            }
            if schema.version < current.schema.version {
                return projection_failure(
                    "projection.schema_rollback",
                    format!(
                        "schema version cannot move backwards: {} is older than the accepted {}",
                        schema.version, current.schema.version
                    ),
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

    fn retain_batch(
        &self,
        batch: &ObservationBatch,
        generation: u64,
        accepted_at_unix: u64,
    ) -> Result<String> {
        let digest = observation_digest(batch)?;
        let path = self
            .paths
            .observation_blob(&self.scope_id, &self.graph_id, &digest)?;
        // Content-addressed files are immutable once written. A replay
        // addresses the same bytes, so an existing blob is a no-op rather
        // than a rewrite.
        if path.exists() {
            return Ok(digest);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let retained = RetainedObservationBatch {
            version: crate::RETAINED_OBSERVATION_VERSION,
            scope_id: self.scope_id.clone(),
            graph_id: self.graph_id.clone(),
            generation,
            accepted_at_unix,
            digest: digest.clone(),
            batch: batch.clone(),
        };
        atomic_write_bytes_locked(&path, &serde_json::to_vec(&retained)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(digest)
    }

    fn retained_batches(&self) -> Result<Vec<(PathBuf, RetainedObservationBatch)>> {
        let dir = self
            .paths
            .observations_dir(&self.scope_id, &self.graph_id)?;
        list_retained(&dir)
    }

    fn graph_dir(&self) -> PathBuf {
        self.paths
            .graph_dir(&self.scope_id, &self.graph_id)
            .unwrap_or_else(|_| self.paths.root().to_path_buf())
    }
}

fn apply_checkpoint_transition(
    checkpoints: &mut NamedCheckpointSet,
    transition: &NamedCheckpointTransition,
) -> Result<()> {
    // Validate every advance before applying any of them: a checkpoint set
    // moves as one value or not at all.
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
                    "checkpoint `{name}` expected {:?}, accepted value is {current:?}",
                    advance.before
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
                format!("inserted vertex `{id}` already exists; replace it instead"),
            );
        }
    }
    for id in &replaced_vertex_ids {
        if !current_vertices.contains_key(id) {
            return projection_failure(
                "projection.replace_missing_vertex",
                format!("replaced vertex `{id}` does not exist; insert it instead"),
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
                format!("removed edge {} does not exist", key.label()),
            );
        }
    }
    for key in &inserted_edge_keys {
        if current_edges.contains_key(key) && !removed_edge_keys.contains(key) {
            return projection_failure(
                "projection.insert_existing_edge",
                format!(
                    "inserted edge {} already exists and was not removed first",
                    key.label()
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
                "observation references require non-empty observation_id, remote_id, \
                 remote_version, and observed_at",
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

fn numbered<T>(values: impl IntoIterator<Item = T>) -> Vec<(usize, T)> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| (index + 1, value))
        .collect()
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

/// A stored snapshot must still be internally consistent. Failing this leaves
/// the store unopenable rather than serving a tampered or truncated
/// generation, and an acceptance that cannot open cannot advance anything.
fn validate_snapshot(snapshot: &SourceProjectionSnapshot) -> Result<()> {
    if snapshot.scope_id.trim().is_empty()
        || snapshot.last_batch_id.trim().is_empty()
        || !is_sha256_hex(&snapshot.last_commit_fingerprint)
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
    let errors = validate_graph(
        &snapshot.descriptor.graph_id,
        GraphSource::ConnectorManaged,
        &snapshot.descriptor,
        &snapshot.schema,
        &numbered(snapshot.vertices.values().cloned()),
        &numbered(snapshot.edges.iter().cloned()),
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

fn fingerprint(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}
