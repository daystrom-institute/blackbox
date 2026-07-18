//! Generic observation-to-graph projection contracts.
//!
//! Observation is intentionally separate from projection. A connector may
//! contact a remote system to construct an [`ObservationBatch`], but a
//! [`GraphProjection`] must be deterministic and replayable without remote
//! access. Durable graph and checkpoint acceptance belongs to
//! `bbox_project_graph::SourceProjectionStore`.

use anyhow::Result;
use bbox_project_graph::{
    GraphDelta, GraphDescriptor, GraphSchema, NamedCheckpointTransition, ReconciliationMode,
    SourceProjectionSnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    pub observation_id: String,
    pub source_entity: String,
    pub remote_id: String,
    pub remote_version: String,
    pub observed_at: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub payload: Option<Value>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionContext {
    pub scope_id: String,
    pub graph_id: String,
    pub prior_generation: Option<u64>,
}

pub trait GraphProjection: Send + Sync {
    fn schema_descriptor(&self) -> GraphSchema;

    fn graph_descriptor(&self, context: &ProjectionContext) -> GraphDescriptor;

    fn project(
        &self,
        context: &ProjectionContext,
        observation_batch: &ObservationBatch,
        prior: Option<&SourceProjectionSnapshot>,
    ) -> Result<GraphDelta>;
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    use anyhow::{Context, bail};
    use bbox_project_graph::{
        CheckpointAdvance, GraphAuthority, GraphEdgeKey, GraphScope, ProjectGraphVertex,
        RetentionPolicy, SourceObservationRef, SourceProjectionStore, VertexTypeDefinition,
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    const CONNECTOR: &str = "synthetic-api";
    const GRAPH_ID: &str = "source-assets";
    const ASSET_TYPE: &str = "dataset:Asset";

    #[derive(Debug, Clone)]
    struct SyntheticDatasetProjection {
        schema_version: u64,
        projection_version: &'static str,
    }

    impl SyntheticDatasetProjection {
        fn generation(context: &ProjectionContext) -> u64 {
            context.prior_generation.unwrap_or(0) + 1
        }

        fn vertex(observation: &ObservationRecord) -> Result<ProjectGraphVertex> {
            let payload = observation
                .payload
                .as_ref()
                .context("live observations require a payload")?;
            Ok(ProjectGraphVertex {
                id: format!("asset:{}", observation.remote_id),
                type_name: ASSET_TYPE.into(),
                label: payload
                    .get("name")
                    .and_then(Value::as_str)
                    .context("asset name must be a string")?
                    .into(),
                properties: BTreeMap::from([
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
                ]),
            })
        }
    }

    impl GraphProjection for SyntheticDatasetProjection {
        fn schema_descriptor(&self) -> GraphSchema {
            GraphSchema {
                version: self.schema_version,
                namespace: "dataset".into(),
                vertex_types: BTreeMap::from([(
                    ASSET_TYPE.into(),
                    VertexTypeDefinition {
                        required: vec![
                            "remote_id".into(),
                            "remote_version".into(),
                            "name".into(),
                            "category".into(),
                        ],
                        properties: BTreeMap::from([
                            ("remote_id".into(), json!("string")),
                            ("remote_version".into(), json!("string")),
                            ("name".into(), json!("string")),
                            ("category".into(), json!("string")),
                        ]),
                    },
                )]),
                edge_types: Vec::new(),
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
                generation: Self::generation(context),
            }
        }

        fn project(
            &self,
            context: &ProjectionContext,
            batch: &ObservationBatch,
            prior: Option<&SourceProjectionSnapshot>,
        ) -> Result<GraphDelta> {
            if batch.source_connector != CONNECTOR {
                bail!("unexpected source connector");
            }
            if prior.map(|snapshot| snapshot.descriptor.generation) != context.prior_generation {
                bail!("projection context does not match prior snapshot");
            }

            let mut current = prior
                .map(|snapshot| snapshot.vertices.clone())
                .unwrap_or_default();
            let before = current.clone();
            let mut observations = batch.observations.clone();
            observations.sort_by(|left, right| {
                left.source_entity
                    .cmp(&right.source_entity)
                    .then(left.remote_id.cmp(&right.remote_id))
                    .then(left.remote_version.cmp(&right.remote_version))
                    .then(left.observation_id.cmp(&right.observation_id))
            });
            let mut seen_remote_ids = BTreeSet::new();
            for observation in &observations {
                if observation.source_entity != "asset" {
                    bail!("unsupported source entity");
                }
                if !seen_remote_ids.insert(observation.remote_id.clone()) {
                    bail!("batch contains duplicate asset identity");
                }
                let vertex_id = format!("asset:{}", observation.remote_id);
                if observation.deleted {
                    current.remove(&vertex_id);
                } else {
                    current.insert(vertex_id, Self::vertex(observation)?);
                }
            }
            if batch.reconciliation_mode == ReconciliationMode::Full {
                current.retain(|id, _| {
                    seen_remote_ids.contains(id.strip_prefix("asset:").unwrap_or(id))
                });
            }

            let inserted_vertices = current
                .iter()
                .filter(|(id, _)| !before.contains_key(*id))
                .map(|(_, vertex)| vertex.clone())
                .collect();
            let replaced_vertices = current
                .iter()
                .filter(|(id, vertex)| before.get(*id).is_some_and(|prior| prior != *vertex))
                .map(|(_, vertex)| vertex.clone())
                .collect();
            let removed_vertex_ids = before
                .keys()
                .filter(|id| !current.contains_key(*id))
                .cloned()
                .collect();

            Ok(GraphDelta {
                graph_id: context.graph_id.clone(),
                batch_id: batch.batch_id.clone(),
                prior_generation: context.prior_generation,
                resulting_generation: Self::generation(context),
                projection_version: self.projection_version.into(),
                reconciliation_mode: batch.reconciliation_mode,
                inserted_vertices,
                replaced_vertices,
                removed_vertex_ids,
                inserted_edges: Vec::new(),
                removed_edges: Vec::<GraphEdgeKey>::new(),
                observations: observations
                    .into_iter()
                    .map(|observation| SourceObservationRef {
                        observation_id: observation.observation_id,
                        remote_id: observation.remote_id,
                        remote_version: observation.remote_version,
                        observed_at: observation.observed_at,
                    })
                    .collect(),
                checkpoint_transition: batch.checkpoint_transition.clone(),
            })
        }
    }

    fn context(prior_generation: Option<u64>) -> ProjectionContext {
        ProjectionContext {
            scope_id: "scope:test".into(),
            graph_id: GRAPH_ID.into(),
            prior_generation,
        }
    }

    fn observation(
        observation_id: &str,
        remote_id: &str,
        remote_version: &str,
        observed_at: &str,
        name: &str,
    ) -> ObservationRecord {
        ObservationRecord {
            observation_id: observation_id.into(),
            source_entity: "asset".into(),
            remote_id: remote_id.into(),
            remote_version: remote_version.into(),
            observed_at: observed_at.into(),
            deleted: false,
            payload: Some(json!({
                "name": name,
                "category": "document"
            })),
        }
    }

    fn deleted(
        observation_id: &str,
        remote_id: &str,
        remote_version: &str,
        observed_at: &str,
    ) -> ObservationRecord {
        ObservationRecord {
            observation_id: observation_id.into(),
            source_entity: "asset".into(),
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
        after: &str,
    ) -> ObservationBatch {
        ObservationBatch {
            batch_id: batch_id.into(),
            source_connector: CONNECTOR.into(),
            source_scope: "dataset:public-fixture".into(),
            reconciliation_mode: mode,
            observations,
            checkpoint_transition: NamedCheckpointTransition {
                advances: BTreeMap::from([(
                    "assets".into(),
                    CheckpointAdvance {
                        before: before.map(str::to_string),
                        after: after.into(),
                    },
                )]),
            },
        }
    }

    fn project_and_accept(
        store: &mut SourceProjectionStore,
        projection: &SyntheticDatasetProjection,
        batch: &ObservationBatch,
    ) -> Result<bbox_project_graph::GraphGeneration> {
        let prior = store.snapshot();
        let projection_context = context(prior.map(|snapshot| snapshot.descriptor.generation));
        let descriptor = projection.graph_descriptor(&projection_context);
        let schema = projection.schema_descriptor();
        let delta = projection.project(&projection_context, batch, prior)?;
        store.accept(&projection_context.scope_id, descriptor, schema, delta)
    }

    #[test]
    fn synthetic_dataset_completes_source_projection_exit_gate() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store_path = root.join("source-assets.json");
        let mut store = SourceProjectionStore::open(&store_path).unwrap();
        let v1 = SyntheticDatasetProjection {
            schema_version: 1,
            projection_version: "dataset-v1",
        };

        let create = batch(
            "batch-1",
            ReconciliationMode::Incremental,
            vec![observation(
                "obs-a1",
                "a",
                "1",
                "2026-01-01T00:00:00Z",
                "Alpha",
            )],
            None,
            "cursor-1",
        );
        let create_delta_a = v1.project(&context(None), &create, None).unwrap();
        let create_delta_b = v1.project(&context(None), &create, None).unwrap();
        assert_eq!(
            serde_json::to_vec(&create_delta_a).unwrap(),
            serde_json::to_vec(&create_delta_b).unwrap()
        );
        let generation = project_and_accept(&mut store, &v1, &create).unwrap();
        assert_eq!(generation.descriptor.generation, 1);
        assert_eq!(generation.vertices["asset:a"].label, "Alpha");

        let update = batch(
            "batch-2",
            ReconciliationMode::Incremental,
            vec![
                observation("obs-b1", "b", "1", "2026-01-02T00:00:00Z", "Beta"),
                observation("obs-a2", "a", "2", "2026-01-02T00:00:00Z", "Alpha revised"),
            ],
            Some("cursor-1"),
            "cursor-2",
        );
        let generation = project_and_accept(&mut store, &v1, &update).unwrap();
        assert_eq!(generation.descriptor.generation, 2);
        assert_eq!(generation.vertices["asset:a"].label, "Alpha revised");
        assert!(generation.vertices.contains_key("asset:b"));

        let delete = batch(
            "batch-3",
            ReconciliationMode::Incremental,
            vec![deleted("obs-a3", "a", "3", "2026-01-03T00:00:00Z")],
            Some("cursor-2"),
            "cursor-3",
        );
        let delete_context = context(Some(2));
        let delete_descriptor = v1.graph_descriptor(&delete_context);
        let delete_schema = v1.schema_descriptor();
        let delete_delta = v1
            .project(&delete_context, &delete, store.snapshot())
            .unwrap();
        let generation = store
            .accept(
                &delete_context.scope_id,
                delete_descriptor.clone(),
                delete_schema.clone(),
                delete_delta.clone(),
            )
            .unwrap();
        assert_eq!(generation.descriptor.generation, 3);
        assert!(!generation.vertices.contains_key("asset:a"));
        assert!(generation.vertices.contains_key("asset:b"));

        let accepted_bytes = fs::read(&store_path).unwrap();
        let replay_context = context(Some(2));
        let replay_delta = v1
            .project(&replay_context, &delete, None)
            .expect_err("replay projection needs its matching prior snapshot");
        assert!(replay_delta.to_string().contains("prior snapshot"));
        let reopened = SourceProjectionStore::open(&store_path).unwrap();
        assert_eq!(reopened.checkpoints().values["assets"], "cursor-3");
        assert_eq!(reopened.status().unwrap().generation, 3);
        assert_eq!(
            reopened.status().unwrap().latest_observed_at.as_deref(),
            Some("2026-01-03T00:00:00Z")
        );

        let mut reopened = reopened;
        let replay_generation = reopened
            .accept(
                &delete_context.scope_id,
                delete_descriptor,
                delete_schema,
                delete_delta,
            )
            .unwrap();
        assert_eq!(replay_generation.descriptor.generation, 3);
        assert_eq!(fs::read(&store_path).unwrap(), accepted_bytes);

        let conflicting = batch(
            "batch-4-conflict",
            ReconciliationMode::Incremental,
            vec![observation(
                "obs-c1",
                "c",
                "1",
                "2026-01-04T00:00:00Z",
                "Gamma",
            )],
            Some("wrong-cursor"),
            "cursor-4",
        );
        let error = project_and_accept(&mut reopened, &v1, &conflicting).unwrap_err();
        assert!(error.to_string().contains("checkpoint"));
        assert_eq!(fs::read(&store_path).unwrap(), accepted_bytes);
        assert_eq!(reopened.status().unwrap().generation, 3);

        let invalid = batch(
            "batch-4-invalid",
            ReconciliationMode::Incremental,
            vec![observation(
                "obs-c1-invalid",
                "c",
                "1",
                "2026-01-04T00:00:00Z",
                "Gamma",
            )],
            Some("cursor-3"),
            "cursor-4",
        );
        let invalid_context = context(Some(3));
        let mut invalid_delta = v1
            .project(&invalid_context, &invalid, reopened.snapshot())
            .unwrap();
        invalid_delta.inserted_vertices[0].label.clear();
        let error = reopened
            .accept(
                &invalid_context.scope_id,
                v1.graph_descriptor(&invalid_context),
                v1.schema_descriptor(),
                invalid_delta,
            )
            .unwrap_err();
        assert!(error.to_string().contains("graph validation"));
        assert_eq!(fs::read(&store_path).unwrap(), accepted_bytes);
        assert_eq!(reopened.checkpoints().values["assets"], "cursor-3");

        let full = batch(
            "batch-4",
            ReconciliationMode::Full,
            vec![observation(
                "obs-c1",
                "c",
                "1",
                "2026-01-04T00:00:00Z",
                "Gamma",
            )],
            Some("cursor-3"),
            "cursor-4",
        );
        let generation = project_and_accept(&mut reopened, &v1, &full).unwrap();
        assert_eq!(generation.descriptor.generation, 4);
        assert!(!generation.vertices.contains_key("asset:b"));
        assert!(generation.vertices.contains_key("asset:c"));
        assert_eq!(
            reopened.status().unwrap().reconciliation_mode,
            ReconciliationMode::Full
        );

        let v2 = SyntheticDatasetProjection {
            schema_version: 2,
            projection_version: "dataset-v2",
        };
        let reprojection = ObservationBatch {
            batch_id: "batch-5-reproject".into(),
            source_connector: CONNECTOR.into(),
            source_scope: "dataset:public-fixture".into(),
            reconciliation_mode: ReconciliationMode::Full,
            observations: vec![observation(
                "obs-c1-replay",
                "c",
                "1",
                "2026-01-04T00:00:00Z",
                "Gamma",
            )],
            checkpoint_transition: NamedCheckpointTransition::default(),
        };
        let generation = project_and_accept(&mut reopened, &v2, &reprojection).unwrap();
        assert_eq!(generation.descriptor.generation, 5);
        assert_eq!(generation.descriptor.schema_version, 2);
        assert_eq!(
            generation.descriptor.projection_version.as_deref(),
            Some("dataset-v2")
        );
        assert_eq!(reopened.checkpoints().values["assets"], "cursor-4");

        let accepted = fs::read(&store_path).unwrap();
        let mut tampered: Value = serde_json::from_slice(&accepted).unwrap();
        tampered["graph_fingerprint"] =
            json!("0000000000000000000000000000000000000000000000000000000000000000");
        fs::write(&store_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        let error = SourceProjectionStore::open(&store_path).unwrap_err();
        assert!(error.to_string().contains("fingerprint"));
    }
}
