//! The M2 exit gate: a synthetic API-dataset connector advances a source
//! graph through create, update, delete, checkpoint resume, and schema
//! reprojection.
//!
//! Everything here is driven by deterministic fixtures. No wire, no remote
//! system, no daemon: observation is the producer plane's job and the typed
//! observation endpoint is M4's, while projection and acceptance are exactly
//! what this crate owns and are replayable without either.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use bbox_project_graph::{
    EdgeEndpointDefinition, EdgeTypeDefinition, GraphAuthority, GraphDescriptor, GraphIndexPolicy,
    GraphSchema, GraphScope, ProjectGraphEdge, ProjectGraphVertex, RetentionPolicy,
    VertexTypeDefinition,
};
use bbox_source_graph::{
    CheckpointAdvance, GraphDelta, GraphEdgeKey, GraphProjection, NamedCheckpointTransition,
    ObservationBatch, ObservationRecord, ObservationRetentionPolicy, ProjectionContext,
    ReconciliationMode, SourceObservationRef, SourceProjectionSnapshot, SourceProjectionStore,
    projection_failure_code,
};

const CONNECTOR: &str = "synthetic-api";
const SCOPE: &str = "connector-source:synthetic-api:fixture-tenant";
const GRAPH_ID: &str = "source-assets";
const ASSET_TYPE: &str = "dataset:Asset";
const OWNER_TYPE: &str = "dataset:Owner";
const OWNED_BY: &str = "dataset:OWNED_BY";

// ---------------------------------------------------------------------------
// The synthetic connector's projection: deterministic, replayable, remote free.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct SyntheticDatasetProjection {
    schema_version: u64,
    projection_version: &'static str,
    /// v2 of this connector's schema carries a region property with a text
    /// index annotation. The annotation is structural: this test only proves
    /// it survives validation and the snapshot round trip.
    with_region: bool,
}

const V1: SyntheticDatasetProjection = SyntheticDatasetProjection {
    schema_version: 1,
    projection_version: "dataset-v1",
    with_region: false,
};

const V2: SyntheticDatasetProjection = SyntheticDatasetProjection {
    schema_version: 2,
    projection_version: "dataset-v2",
    with_region: true,
};

type VertexMap = BTreeMap<String, ProjectGraphVertex>;
type EdgeMap = BTreeMap<GraphEdgeKey, ProjectGraphEdge>;

impl SyntheticDatasetProjection {
    fn asset_vertex(&self, observation: &ObservationRecord) -> Result<ProjectGraphVertex> {
        let Some(payload) = observation.payload.as_ref() else {
            bail!("a live observation requires a payload");
        };
        let mut properties = BTreeMap::from([
            ("remote_id".into(), json!(observation.remote_id)),
            ("remote_version".into(), json!(observation.remote_version)),
            ("name".into(), payload["name"].clone()),
            (
                "category".into(),
                payload
                    .get("category")
                    .cloned()
                    .unwrap_or_else(|| json!("unclassified")),
            ),
            (
                "owner".into(),
                payload
                    .get("owner")
                    .cloned()
                    .unwrap_or_else(|| json!("unassigned")),
            ),
        ]);
        if self.with_region {
            properties.insert(
                "region".into(),
                payload
                    .get("region")
                    .cloned()
                    .unwrap_or_else(|| json!("unknown")),
            );
        }
        Ok(ProjectGraphVertex {
            id: format!("asset:{}", observation.remote_id),
            type_name: ASSET_TYPE.into(),
            label: payload["name"].as_str().unwrap_or_default().into(),
            properties,
        })
    }

    fn owner_vertex(&self, observation: &ObservationRecord) -> Result<ProjectGraphVertex> {
        let Some(payload) = observation.payload.as_ref() else {
            bail!("a live observation requires a payload");
        };
        Ok(ProjectGraphVertex {
            id: format!("owner:{}", observation.remote_id),
            type_name: OWNER_TYPE.into(),
            label: payload["name"].as_str().unwrap_or_default().into(),
            properties: BTreeMap::from([
                ("remote_id".into(), json!(observation.remote_id)),
                ("name".into(), payload["name"].clone()),
            ]),
        })
    }

    /// Fold one batch into a vertex set. Sorting the batch first is what makes
    /// the projection deterministic for the same accepted observations.
    fn apply(&self, vertices: &mut VertexMap, batch: &ObservationBatch) -> Result<()> {
        if batch.source_connector != CONNECTOR {
            bail!("unexpected source connector");
        }
        let mut observations = batch.observations.clone();
        observations.sort_by(|left, right| {
            left.source_entity
                .cmp(&right.source_entity)
                .then(left.remote_id.cmp(&right.remote_id))
                .then(left.remote_version.cmp(&right.remote_version))
                .then(left.observation_id.cmp(&right.observation_id))
        });
        let mut observed = BTreeSet::new();
        for observation in &observations {
            let vertex_id = match observation.source_entity.as_str() {
                "asset" => format!("asset:{}", observation.remote_id),
                "owner" => format!("owner:{}", observation.remote_id),
                other => bail!("unsupported source entity `{other}`"),
            };
            if !observed.insert(vertex_id.clone()) {
                bail!("batch contains duplicate identity `{vertex_id}`");
            }
            if observation.deleted {
                vertices.remove(&vertex_id);
                continue;
            }
            let vertex = match observation.source_entity.as_str() {
                "asset" => self.asset_vertex(observation)?,
                _ => self.owner_vertex(observation)?,
            };
            vertices.insert(vertex_id, vertex);
        }
        if batch.reconciliation_mode == ReconciliationMode::Full {
            // Full reconciliation observed the complete source scope, so
            // anything unmentioned is gone.
            vertices.retain(|id, _| observed.contains(id));
        }
        Ok(())
    }

    /// Edges are derived from facts, never observed directly: an asset is
    /// owned by the owner its payload names, when that owner exists.
    fn derive_edges(&self, vertices: &VertexMap) -> EdgeMap {
        vertices
            .values()
            .filter(|vertex| vertex.type_name == ASSET_TYPE)
            .filter_map(|asset| {
                let owner = asset.properties.get("owner")?.as_str()?;
                let owner_id = format!("owner:{owner}");
                vertices.contains_key(&owner_id).then(|| {
                    let edge = ProjectGraphEdge {
                        from: asset.id.clone(),
                        type_name: OWNED_BY.into(),
                        to: owner_id,
                        properties: BTreeMap::new(),
                    };
                    (GraphEdgeKey::of(&edge), edge)
                })
            })
            .collect()
    }

    fn prior_state(&self, prior: Option<&SourceProjectionSnapshot>) -> (VertexMap, EdgeMap) {
        let vertices = prior
            .map(|snapshot| snapshot.vertices.clone())
            .unwrap_or_default();
        let edges = prior
            .map(|snapshot| {
                snapshot
                    .edges
                    .iter()
                    .map(|edge| (GraphEdgeKey::of(edge), edge.clone()))
                    .collect()
            })
            .unwrap_or_default();
        (vertices, edges)
    }

    #[allow(clippy::too_many_arguments)]
    fn delta_between(
        &self,
        context: &ProjectionContext,
        batch_id: &str,
        mode: ReconciliationMode,
        before: (&VertexMap, &EdgeMap),
        after: (&VertexMap, &EdgeMap),
        observations: Vec<SourceObservationRef>,
        transition: NamedCheckpointTransition,
    ) -> GraphDelta {
        let (before_vertices, before_edges) = before;
        let (after_vertices, after_edges) = after;
        GraphDelta {
            graph_id: context.graph_id.clone(),
            batch_id: batch_id.to_string(),
            prior_generation: context.prior_generation,
            resulting_generation: context.resulting_generation(),
            projection_version: self.projection_version.into(),
            reconciliation_mode: mode,
            inserted_vertices: after_vertices
                .iter()
                .filter(|(id, _)| !before_vertices.contains_key(*id))
                .map(|(_, vertex)| vertex.clone())
                .collect(),
            replaced_vertices: after_vertices
                .iter()
                .filter(|(id, vertex)| {
                    before_vertices
                        .get(*id)
                        .is_some_and(|prior| prior != *vertex)
                })
                .map(|(_, vertex)| vertex.clone())
                .collect(),
            removed_vertex_ids: before_vertices
                .keys()
                .filter(|id| !after_vertices.contains_key(*id))
                .cloned()
                .collect(),
            inserted_edges: after_edges
                .iter()
                .filter(|(key, _)| !before_edges.contains_key(*key))
                .map(|(_, edge)| edge.clone())
                .collect(),
            removed_edges: before_edges
                .keys()
                .filter(|key| !after_edges.contains_key(*key))
                .cloned()
                .collect(),
            observations,
            checkpoint_transition: transition,
            allow_empty_full_reconciliation: false,
        }
    }

    /// Rebuild the whole graph from retained observations. This is the shape
    /// of a schema reprojection: replay corpus side, then accept the result as
    /// exactly one new generation.
    fn reproject(
        &self,
        context: &ProjectionContext,
        prior: Option<&SourceProjectionSnapshot>,
        batches: &[ObservationBatch],
        batch_id: &str,
    ) -> Result<GraphDelta> {
        let mut vertices = VertexMap::new();
        for batch in batches {
            self.apply(&mut vertices, batch)?;
        }
        let edges = self.derive_edges(&vertices);
        let (before_vertices, before_edges) = self.prior_state(prior);
        Ok(self.delta_between(
            context,
            batch_id,
            ReconciliationMode::Full,
            (&before_vertices, &before_edges),
            (&vertices, &edges),
            batches
                .iter()
                .flat_map(|batch| batch.observations.iter())
                .map(observation_ref)
                .collect(),
            NamedCheckpointTransition::default(),
        ))
    }
}

impl GraphProjection for SyntheticDatasetProjection {
    fn schema_descriptor(&self) -> GraphSchema {
        let mut asset_required = vec![
            "remote_id".to_string(),
            "remote_version".to_string(),
            "name".to_string(),
            "category".to_string(),
            "owner".to_string(),
        ];
        let mut asset_properties = BTreeMap::from([
            ("remote_id".to_string(), json!("string")),
            ("remote_version".to_string(), json!("string")),
            ("name".to_string(), json!("string")),
            ("category".to_string(), json!("string")),
            ("owner".to_string(), json!("string")),
        ]);
        if self.with_region {
            asset_required.push("region".into());
            // The per-property retrieval annotation from decision
            // b1a11d7cf59f2545. Accepted and preserved; nothing reads it yet.
            asset_properties.insert(
                "region".into(),
                json!({"type": "string", "index": "text", "embed": false}),
            );
        }
        GraphSchema {
            version: self.schema_version,
            namespace: "dataset".into(),
            vertex_types: BTreeMap::from([
                (
                    ASSET_TYPE.to_string(),
                    VertexTypeDefinition {
                        required: asset_required,
                        properties: asset_properties,
                    },
                ),
                (
                    OWNER_TYPE.to_string(),
                    VertexTypeDefinition {
                        required: vec!["remote_id".into(), "name".into()],
                        properties: BTreeMap::from([
                            ("remote_id".to_string(), json!("string")),
                            ("name".to_string(), json!("string")),
                        ]),
                    },
                ),
            ]),
            edge_types: vec![EdgeTypeDefinition {
                type_name: OWNED_BY.into(),
                endpoints: vec![EdgeEndpointDefinition {
                    from_type: ASSET_TYPE.into(),
                    to_type: OWNER_TYPE.into(),
                }],
                required: Vec::new(),
                properties: BTreeMap::new(),
            }],
            index_policy: GraphIndexPolicy::default(),
        }
    }

    fn graph_descriptor(&self, context: &ProjectionContext) -> GraphDescriptor {
        GraphDescriptor {
            descriptor_version: 1,
            scope: GraphScope::Project,
            graph_id: context.graph_id.clone(),
            authority: GraphAuthority::Connector,
            schema_id: "synthetic-dataset-schema".into(),
            schema_version: self.schema_version,
            projection_version: Some(self.projection_version.into()),
            source_connector: Some(CONNECTOR.into()),
            retention_policy: RetentionPolicy::ConnectorManaged,
            generation: context.resulting_generation(),
        }
    }

    fn project(
        &self,
        context: &ProjectionContext,
        batch: &ObservationBatch,
        prior: Option<&SourceProjectionSnapshot>,
    ) -> Result<GraphDelta> {
        if prior.map(|snapshot| snapshot.descriptor.generation) != context.prior_generation {
            bail!("projection context does not match the prior snapshot");
        }
        let (before_vertices, before_edges) = self.prior_state(prior);
        let mut vertices = before_vertices.clone();
        self.apply(&mut vertices, batch)?;
        let edges = self.derive_edges(&vertices);
        Ok(self.delta_between(
            context,
            &batch.batch_id,
            batch.reconciliation_mode,
            (&before_vertices, &before_edges),
            (&vertices, &edges),
            batch.observations.iter().map(observation_ref).collect(),
            batch.checkpoint_transition.clone(),
        ))
    }
}

fn observation_ref(observation: &ObservationRecord) -> SourceObservationRef {
    SourceObservationRef {
        observation_id: observation.observation_id.clone(),
        remote_id: observation.remote_id.clone(),
        remote_version: observation.remote_version.clone(),
        observed_at: observation.observed_at.clone(),
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn store_root() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("source-graphs");
    (temp, root)
}

fn context(prior_generation: Option<u64>) -> ProjectionContext {
    ProjectionContext {
        scope_id: SCOPE.into(),
        graph_id: GRAPH_ID.into(),
        prior_generation,
    }
}

fn observation(
    observation_id: &str,
    entity: &str,
    remote_id: &str,
    remote_version: &str,
    observed_at: &str,
    payload: Value,
) -> ObservationRecord {
    ObservationRecord {
        observation_id: observation_id.into(),
        source_entity: entity.into(),
        remote_id: remote_id.into(),
        remote_version: remote_version.into(),
        observed_at: observed_at.into(),
        deleted: false,
        payload: Some(payload),
    }
}

fn asset(
    observation_id: &str,
    remote_id: &str,
    remote_version: &str,
    observed_at: &str,
    name: &str,
    owner: &str,
) -> ObservationRecord {
    observation(
        observation_id,
        "asset",
        remote_id,
        remote_version,
        observed_at,
        json!({"name": name, "category": "document", "owner": owner, "region": "north"}),
    )
}

fn owner(
    observation_id: &str,
    remote_id: &str,
    observed_at: &str,
    name: &str,
) -> ObservationRecord {
    observation(
        observation_id,
        "owner",
        remote_id,
        "1",
        observed_at,
        json!({"name": name}),
    )
}

fn deleted(
    observation_id: &str,
    entity: &str,
    remote_id: &str,
    remote_version: &str,
    observed_at: &str,
) -> ObservationRecord {
    ObservationRecord {
        observation_id: observation_id.into(),
        source_entity: entity.into(),
        remote_id: remote_id.into(),
        remote_version: remote_version.into(),
        observed_at: observed_at.into(),
        deleted: true,
        payload: None,
    }
}

fn batch(
    batch_id: &str,
    mode: ReconciliationMode,
    observations: Vec<ObservationRecord>,
    before: Option<&str>,
    after: Option<&str>,
) -> ObservationBatch {
    ObservationBatch {
        batch_id: batch_id.into(),
        source_connector: CONNECTOR.into(),
        source_scope: "dataset:public-fixture".into(),
        reconciliation_mode: mode,
        observations,
        checkpoint_transition: NamedCheckpointTransition {
            advances: match after {
                Some(after) => BTreeMap::from([(
                    "assets".to_string(),
                    CheckpointAdvance {
                        before: before.map(str::to_string),
                        after: after.into(),
                    },
                )]),
                None => BTreeMap::new(),
            },
        },
    }
}

/// Project one batch against the store's accepted generation and accept the
/// result, retaining the batch for later reprojection.
fn advance(
    store: &mut SourceProjectionStore,
    projection: &SyntheticDatasetProjection,
    batch: &ObservationBatch,
) -> Result<bbox_project_graph::GraphGeneration> {
    let context = context(store.accepted_generation_number());
    let descriptor = projection.graph_descriptor(&context);
    let schema = projection.schema_descriptor();
    let delta = projection.project(&context, batch, store.snapshot())?;
    store.accept(descriptor, schema, delta, Some(batch))
}

fn snapshot_bytes(root: &Path) -> Vec<u8> {
    let path = bbox_source_graph::SourceProjectionPaths::new(root)
        .snapshot(SCOPE, GRAPH_ID)
        .unwrap();
    fs::read(path).unwrap()
}

fn create_batch() -> ObservationBatch {
    batch(
        "batch-1",
        ReconciliationMode::Incremental,
        vec![
            owner("obs-o1", "o1", "2026-01-01T00:00:00Z", "Fixture Owner"),
            asset("obs-a1", "a", "1", "2026-01-01T00:00:00Z", "Alpha", "o1"),
        ],
        None,
        Some("cursor-1"),
    )
}

fn update_batch() -> ObservationBatch {
    batch(
        "batch-2",
        ReconciliationMode::Incremental,
        vec![
            asset(
                "obs-a2",
                "a",
                "2",
                "2026-01-02T00:00:00Z",
                "Alpha revised",
                "o1",
            ),
            asset("obs-b1", "b", "1", "2026-01-02T00:00:00Z", "Beta", "o1"),
        ],
        Some("cursor-1"),
        Some("cursor-2"),
    )
}

fn delete_batch() -> ObservationBatch {
    batch(
        "batch-3",
        ReconciliationMode::Incremental,
        vec![deleted("obs-a3", "asset", "a", "3", "2026-01-03T00:00:00Z")],
        Some("cursor-2"),
        Some("cursor-3"),
    )
}

// ---------------------------------------------------------------------------
// The exit gate
// ---------------------------------------------------------------------------

/// M2 exit gate: create, update, delete, checkpoint resume, and schema
/// reprojection, driven end to end by fixtures.
#[test]
fn a_synthetic_connector_advances_a_source_graph_through_the_whole_lifecycle() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    assert!(store.status().is_none(), "an empty store has no status");

    // CREATE. The projection is deterministic for the same inputs.
    let create = create_batch();
    let first = V1.project(&context(None), &create, None).unwrap();
    let second = V1.project(&context(None), &create, None).unwrap();
    assert_eq!(first, second, "projection must be deterministic");

    let generation = advance(&mut store, &V1, &create).unwrap();
    assert_eq!(generation.descriptor.generation, 1);
    assert_eq!(generation.vertices["asset:a"].label, "Alpha");
    assert!(
        generation
            .edges
            .iter()
            .any(|edge| edge.type_name == OWNED_BY && edge.to == "owner:o1"),
        "the derived ownership edge is part of the accepted generation"
    );
    assert_eq!(store.checkpoints().get("assets"), Some("cursor-1"));

    // UPDATE: one replaced vertex, one inserted vertex, one new edge.
    let generation = advance(&mut store, &V1, &update_batch()).unwrap();
    assert_eq!(generation.descriptor.generation, 2);
    assert_eq!(generation.vertices["asset:a"].label, "Alpha revised");
    assert!(generation.vertices.contains_key("asset:b"));
    assert_eq!(store.checkpoints().get("assets"), Some("cursor-2"));

    // DELETE: the vertex and its derived edge leave together.
    let generation = advance(&mut store, &V1, &delete_batch()).unwrap();
    assert_eq!(generation.descriptor.generation, 3);
    assert!(!generation.vertices.contains_key("asset:a"));
    assert!(generation.vertices.contains_key("asset:b"));
    assert!(
        !generation
            .edges
            .iter()
            .any(|edge| edge.from == "asset:a" && edge.type_name == OWNED_BY),
        "removing a vertex must remove its derived edge in the same delta"
    );

    // CHECKPOINT RESUME: a fresh store instance answers from the accepted
    // snapshot alone, and the next batch resumes from the accepted cursor.
    drop(store);
    let mut resumed = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    let status = resumed.status().unwrap();
    assert_eq!(status.generation, 3);
    assert_eq!(status.checkpoints.get("assets"), Some("cursor-3"));
    assert_eq!(
        status.latest_observed_at.as_deref(),
        Some("2026-01-03T00:00:00Z")
    );

    let resume_cursor = resumed.checkpoints().get("assets").unwrap().to_string();
    let resume = batch(
        "batch-4",
        ReconciliationMode::Full,
        vec![
            owner("obs-o1b", "o1", "2026-01-04T00:00:00Z", "Fixture Owner"),
            asset("obs-c1", "c", "1", "2026-01-04T00:00:00Z", "Gamma", "o1"),
        ],
        Some(&resume_cursor),
        Some("cursor-4"),
    );
    let generation = advance(&mut resumed, &V1, &resume).unwrap();
    assert_eq!(generation.descriptor.generation, 4);
    assert!(
        !generation.vertices.contains_key("asset:b"),
        "full reconciliation removes what it did not observe"
    );
    assert!(generation.vertices.contains_key("asset:c"));
    assert_eq!(
        resumed.status().unwrap().reconciliation_mode,
        ReconciliationMode::Full
    );

    // SCHEMA REPROJECTION: replay the retained observations under v2 and
    // accept the rebuild as exactly one more generation.
    let plan = resumed.replay_plan(0).unwrap();
    let retained = plan
        .batches
        .iter()
        .map(|item| resumed.load_retained_batch(&item.digest).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 4, "every accepted batch was retained");

    let reprojection_context = context(resumed.accepted_generation_number());
    let delta = V2
        .reproject(
            &reprojection_context,
            resumed.snapshot(),
            &retained,
            "batch-5-reproject",
        )
        .unwrap();
    let generation = resumed
        .accept(
            V2.graph_descriptor(&reprojection_context),
            V2.schema_descriptor(),
            delta,
            None,
        )
        .unwrap();

    assert_eq!(generation.descriptor.generation, 5);
    assert_eq!(generation.descriptor.schema_version, 2);
    assert_eq!(
        generation.descriptor.projection_version.as_deref(),
        Some("dataset-v2")
    );
    assert_eq!(
        generation.vertices["asset:c"].properties["region"],
        json!("north"),
        "the reprojection rebuilt facts under the new schema"
    );
    assert!(
        !generation.vertices.contains_key("asset:b"),
        "the replayed history still ends at the full reconciliation"
    );
    assert_eq!(
        resumed.checkpoints().get("assets"),
        Some("cursor-4"),
        "a reprojection carries no checkpoint transition and leaves the set alone"
    );

    // The v2 annotation survived acceptance structurally.
    let region_term = &generation.schema.vertex_types[ASSET_TYPE].properties["region"];
    assert!(bbox_project_graph::is_annotated_property_term(region_term));
    assert_eq!(
        bbox_project_graph::property_annotations(region_term).index,
        bbox_project_graph::PropertyIndexMode::Text
    );
}

/// Generations advance by exactly one, and only an exact replay of the most
/// recently accepted batch is idempotent.
#[test]
fn only_an_exact_replay_of_the_last_accepted_batch_is_idempotent() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    advance(&mut store, &V1, &create_batch()).unwrap();
    advance(&mut store, &V1, &update_batch()).unwrap();
    let accepted = snapshot_bytes(&root);

    // Exact replay: same generation, byte-identical store.
    let replay_context = context(Some(1));
    let replay_delta = V1
        .project(
            &replay_context,
            &update_batch(),
            store.prior_snapshot().unwrap().as_ref(),
        )
        .unwrap();
    let generation = store
        .accept(
            V1.graph_descriptor(&replay_context),
            V1.schema_descriptor(),
            replay_delta,
            Some(&update_batch()),
        )
        .unwrap();
    assert_eq!(generation.descriptor.generation, 2);
    assert_eq!(snapshot_bytes(&root), accepted);

    // Same batch id, different content: refused.
    let mut divergent = V1
        .project(
            &replay_context,
            &update_batch(),
            store.prior_snapshot().unwrap().as_ref(),
        )
        .unwrap();
    divergent.projection_version = "dataset-v1".into();
    divergent.inserted_vertices[0].label = "tampered".into();
    let error = store
        .accept(
            V1.graph_descriptor(&replay_context),
            V1.schema_descriptor(),
            divergent,
            None,
        )
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.batch_conflict")
    );
    assert_eq!(snapshot_bytes(&root), accepted);

    // A delta that skips a generation is refused.
    let mut skipping = V1
        .project(&context(Some(2)), &delete_batch(), store.snapshot())
        .unwrap();
    skipping.resulting_generation = 4;
    let mut descriptor = V1.graph_descriptor(&context(Some(2)));
    descriptor.generation = 4;
    let error = store
        .accept(descriptor, V1.schema_descriptor(), skipping, None)
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.non_monotonic_generation")
    );
    assert_eq!(snapshot_bytes(&root), accepted);

    // A delta computed against a stale prior generation is refused.
    let stale = V1
        .project(&context(Some(1)), &delete_batch(), store.snapshot())
        .unwrap_err();
    assert!(stale.to_string().contains("prior snapshot"));
}

/// Every rejection class leaves the accepted generation and the accepted
/// checkpoint set exactly where they were.
#[test]
fn a_rejected_acceptance_changes_neither_the_generation_nor_the_checkpoints() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    advance(&mut store, &V1, &create_batch()).unwrap();
    advance(&mut store, &V1, &update_batch()).unwrap();
    let accepted = snapshot_bytes(&root);

    let assert_unchanged = |store: &SourceProjectionStore| {
        assert_eq!(snapshot_bytes(&root), accepted);
        assert_eq!(store.accepted_generation_number(), Some(2));
        assert_eq!(store.checkpoints().get("assets"), Some("cursor-2"));
    };

    // 1. Rejected graph validation.
    let invalid_context = context(Some(2));
    let mut invalid = V1
        .project(&invalid_context, &delete_batch(), store.snapshot())
        .unwrap();
    invalid.removed_vertex_ids.clear();
    invalid.removed_edges.clear();
    invalid.replaced_vertices.push(ProjectGraphVertex {
        id: "asset:b".into(),
        type_name: ASSET_TYPE.into(),
        label: String::new(),
        properties: BTreeMap::new(),
    });
    let error = store
        .accept(
            V1.graph_descriptor(&invalid_context),
            V1.schema_descriptor(),
            invalid,
            None,
        )
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.graph_invalid")
    );
    assert_unchanged(&store);

    // 2. Checkpoint conflict: the producer's `before` does not match.
    let conflicting = batch(
        "batch-conflict",
        ReconciliationMode::Incremental,
        vec![asset(
            "obs-d1",
            "d",
            "1",
            "2026-01-05T00:00:00Z",
            "Delta",
            "o1",
        )],
        Some("wrong-cursor"),
        Some("cursor-9"),
    );
    let error = advance(&mut store, &V1, &conflicting).unwrap_err();
    assert_eq!(projection_failure_code(&error), Some("checkpoint.conflict"));
    assert_unchanged(&store);

    // 3. Schema rollback.
    let rollback_context = context(Some(2));
    let older = SyntheticDatasetProjection {
        schema_version: 0,
        projection_version: "dataset-v0",
        with_region: false,
    };
    let mut delta = V1
        .project(&rollback_context, &delete_batch(), store.snapshot())
        .unwrap();
    delta.projection_version = "dataset-v0".into();
    delta.batch_id = "batch-rollback".into();
    let error = store
        .accept(
            older.graph_descriptor(&rollback_context),
            older.schema_descriptor(),
            delta,
            None,
        )
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.schema_rollback")
    );
    assert_unchanged(&store);

    // 4. Snapshot integrity failure: a tampered fingerprint refuses to open,
    //    so nothing can be accepted against it.
    let path = bbox_source_graph::SourceProjectionPaths::new(&root)
        .snapshot(SCOPE, GRAPH_ID)
        .unwrap();
    let mut tampered: Value = serde_json::from_slice(&accepted).unwrap();
    tampered["graph_fingerprint"] = json!("0".repeat(64));
    fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let error = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.snapshot_fingerprint_mismatch")
    );
    fs::write(&path, &accepted).unwrap();
    assert!(SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).is_ok());
}

/// A full reconciliation that observed nothing cannot be allowed to look like
/// an empty remote source.
#[test]
fn an_empty_full_reconciliation_needs_an_explicit_assertion() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    advance(&mut store, &V1, &create_batch()).unwrap();

    let wipe_context = context(Some(1));
    let mut wipe = V1
        .project(
            &wipe_context,
            &batch(
                "batch-empty-full",
                ReconciliationMode::Full,
                Vec::new(),
                Some("cursor-1"),
                Some("cursor-2"),
            ),
            store.snapshot(),
        )
        .unwrap();
    assert!(!wipe.removed_vertex_ids.is_empty());

    let error = store
        .accept(
            V1.graph_descriptor(&wipe_context),
            V1.schema_descriptor(),
            wipe.clone(),
            None,
        )
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.empty_full_reconciliation")
    );
    assert_eq!(store.accepted_generation_number(), Some(1));

    wipe.allow_empty_full_reconciliation = true;
    let generation = store
        .accept(
            V1.graph_descriptor(&wipe_context),
            V1.schema_descriptor(),
            wipe,
            None,
        )
        .unwrap();
    assert_eq!(generation.descriptor.generation, 2);
    assert!(
        generation
            .vertices
            .values()
            .all(|vertex| { vertex.type_name != ASSET_TYPE && vertex.type_name != OWNER_TYPE })
    );
}

/// Removing a vertex does not cascade to its edges: a delta that strands one
/// is refused by name.
#[test]
fn a_delta_that_strands_an_edge_is_refused_by_name() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    advance(&mut store, &V1, &create_batch()).unwrap();

    let strand_context = context(Some(1));
    let mut delta = V1
        .project(
            &strand_context,
            &batch(
                "batch-strand",
                ReconciliationMode::Incremental,
                vec![deleted(
                    "obs-o1d",
                    "owner",
                    "o1",
                    "2",
                    "2026-01-06T00:00:00Z",
                )],
                Some("cursor-1"),
                Some("cursor-2"),
            ),
            store.snapshot(),
        )
        .unwrap();
    // Keep the owner removal but forget the edge that pointed at it.
    delta.removed_edges.clear();
    let error = store
        .accept(
            V1.graph_descriptor(&strand_context),
            V1.schema_descriptor(),
            delta,
            None,
        )
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.dangling_edge_after_removal")
    );
    assert_eq!(store.accepted_generation_number(), Some(1));
}

/// Retention keeps the current and prior generation plus the window; a replay
/// past the horizon reports itself incomplete rather than projecting a partial
/// history.
#[test]
fn reprojection_degrades_honestly_past_the_retention_horizon() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open_with_retention(
        &root,
        SCOPE,
        GRAPH_ID,
        ObservationRetentionPolicy {
            retained_generations: 2,
            retention_window_secs: 0,
        },
    )
    .unwrap();
    advance(&mut store, &V1, &create_batch()).unwrap();
    advance(&mut store, &V1, &update_batch()).unwrap();
    advance(&mut store, &V1, &delete_batch()).unwrap();

    let plan = store.replay_plan(0).unwrap();
    assert!(plan.complete, "everything is still retained: {plan:?}");
    assert_eq!(plan.batches.len(), 3);

    // Sweep far in the future so only the generation window protects
    // anything.
    let stats = store.sweep_retained_observations(u64::MAX / 2).unwrap();
    assert_eq!(stats.examined, 3);
    assert_eq!(stats.reclaimed, 1, "generation 1 fell outside both windows");

    let plan = store.replay_plan(0).unwrap();
    assert!(
        !plan.complete,
        "a replay from the beginning is no longer possible: {plan:?}"
    );
    assert_eq!(plan.earliest_retained_generation, Some(2));

    let plan = store.replay_plan(1).unwrap();
    assert!(plan.complete, "a replay from generation 1 still works");
    assert_eq!(plan.batches.len(), 2);

    // The accepted generation is untouched by retention work.
    assert_eq!(store.accepted_generation_number(), Some(3));
    let surviving = plan.batches[0].digest.clone();
    assert!(store.load_retained_batch(&surviving).is_ok());
}

/// Status is freshness and identity only. No payload, no credential material.
#[test]
fn status_exposes_freshness_without_credential_material() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    let mut leaky = create_batch();
    leaky.observations.push(observation(
        "obs-secret",
        "asset",
        "s",
        "1",
        "2026-01-01T00:00:00Z",
        json!({
            "name": "Secret Asset",
            "category": "document",
            "owner": "o1",
            "api_token": "fixture-token-do-not-leak"
        }),
    ));
    // The projection never copies unknown payload fields into facts, so the
    // token cannot reach the graph in the first place.
    advance(&mut store, &V1, &leaky).unwrap();

    let status = store.status().unwrap();
    assert_eq!(status.generation, 1);
    assert_eq!(status.source_connector, CONNECTOR);
    assert_eq!(status.projection_version, "dataset-v1");
    assert_eq!(status.schema_version, 1);
    assert_eq!(status.checkpoints.get("assets"), Some("cursor-1"));
    assert_eq!(status.retained_observation_count, 1);
    assert!(!status.graph_fingerprint.is_empty());

    let rendered = serde_json::to_string(&status).unwrap();
    assert!(
        !rendered.contains("fixture-token-do-not-leak"),
        "status must never carry observation payload: {rendered}"
    );
    let accepted = String::from_utf8(snapshot_bytes(&root)).unwrap();
    assert!(
        !accepted.contains("fixture-token-do-not-leak"),
        "graph facts must never carry unmodelled payload fields"
    );
}

/// The store accepts connector authority only, so a connector refresh has no
/// path to a project-authored graph.
#[test]
fn the_source_projection_store_refuses_project_authority() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    let create = create_batch();
    let context = context(None);
    let delta = V1.project(&context, &create, None).unwrap();
    let mut descriptor = V1.graph_descriptor(&context);
    descriptor.authority = GraphAuthority::Project;
    descriptor.projection_version = None;
    descriptor.source_connector = None;
    descriptor.retention_policy = RetentionPolicy::ProjectOwned;

    let error = store
        .accept(descriptor, V1.schema_descriptor(), delta, Some(&create))
        .unwrap_err();
    assert_eq!(
        projection_failure_code(&error),
        Some("projection.authority_required")
    );
    assert_eq!(store.accepted_generation_number(), None);
}

/// The prior generation is retained beside the accepted one for diagnosis and
/// is never the authority.
#[test]
fn the_prior_generation_is_retained_for_diagnosis() {
    let (_temp, root) = store_root();
    let mut store = SourceProjectionStore::open(&root, SCOPE, GRAPH_ID).unwrap();
    advance(&mut store, &V1, &create_batch()).unwrap();
    assert!(
        store.prior_snapshot().unwrap().is_none(),
        "the first accepted generation has no prior"
    );

    advance(&mut store, &V1, &update_batch()).unwrap();
    let prior = store.prior_snapshot().unwrap().expect("prior is retained");
    assert_eq!(prior.descriptor.generation, 1);
    assert_eq!(prior.checkpoints.get("assets"), Some("cursor-1"));
    assert_eq!(store.accepted_generation_number(), Some(2));
    assert!(store.status().unwrap().prior_generation_available);
}
