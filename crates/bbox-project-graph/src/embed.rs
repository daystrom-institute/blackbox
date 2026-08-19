//! Schema-directed embedding projection for graph vertices
//! (unified-retrieval design 4.4 / 7.5, M9e).
//!
//! The vector lane never embeds a vertex's raw JSON. It embeds the composed
//! embed-eligible projection: the label, then the values of the properties
//! whose schema term annotates `embed: true`, in schema order. Participation
//! is the three-way gate from decision `b1a11d7cf59f2545`: the graph policy
//! enables embeddings, the property opts in, and the vertex type is not
//! excluded by policy. Meta vertices (schema-as-data) never embed: authors
//! search their facts, not the shape of their schema.
//!
//! This module is pure data over the in-memory generation so the three
//! consumers agree byte-for-byte: the index-time enqueue, the backfill
//! route, and the query-time authority re-check that drops a vector whose
//! vertex is no longer embed-eligible on the accepted generation.

use sha2::{Digest, Sha256};

use crate::{
    FIXED_META_VERTICES, GraphGeneration, META_EDGE_TYPE, META_VERTEX_TYPE, ProjectGraphVertex,
    property_annotations, property_value_string,
};

/// Version of the graph-vertex embedding-input composition. The embed queue
/// dedups on `(entity_id, content_hash)` and the content hash is the
/// versioned envelope over the composed text, so bumping this constant is
/// the ONLY mechanism that re-embeds an unchanged vertex after the
/// composition changes. Never fold a composition change in silently.
pub const GRAPH_EMBED_TEXT_VERSION: &str = "graph-vertex-embed-text-v1-label-embed-props";

/// Whether a projected vertex is schema-as-data rather than an authored fact.
/// The fixed meta floor, the per-type definition vertices, and anything else
/// typed as a meta vertex never reach the word index or the vector lane.
pub fn is_meta_vertex(vertex: &ProjectGraphVertex) -> bool {
    FIXED_META_VERTICES.contains(&vertex.id.as_str())
        || vertex.type_name == META_VERTEX_TYPE
        || vertex.type_name == META_EDGE_TYPE
}

/// One vertex's embed-eligible projection, ready for the embed queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphVertexEmbedProjection {
    pub vertex_id: String,
    pub vertex_type: String,
    /// The composed embedding input: label, then `embed: true` property
    /// values in schema order, newline-joined.
    pub text: String,
}

impl GraphVertexEmbedProjection {
    /// The versioned envelope hash the embed queue dedups on:
    /// `sha256(GRAPH_EMBED_TEXT_VERSION || text)`. Every boundary that
    /// compares graph vector freshness (enqueue, coverage, the describe
    /// participation report) must use this same envelope.
    pub fn content_hash(&self) -> String {
        graph_vertex_embed_content_hash(&self.text)
    }
}

/// The versioned envelope hash over one composed embedding input.
pub fn graph_vertex_embed_content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(GRAPH_EMBED_TEXT_VERSION.as_bytes());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Whether the graph's policy lets ANY vertex embed. Local-scratch sources
/// are never indexable and never embed; the rest is the per-graph gate.
pub fn graph_embeds(generation: &GraphGeneration) -> bool {
    generation.schema.index_policy.embeddings_enabled
        && !matches!(generation.key.source, crate::GraphSource::LocalScratch)
}

/// The composed embed-eligible projection of one vertex under its
/// generation's schema and policy, or `None` when the vertex does not
/// participate: policy disabled, meta vertex, excluded type, no `embed: true`
/// property, or every opted-in property absent on this vertex (a label alone
/// is not worth a vector; the word lane already carries it).
pub fn vertex_embed_text(
    generation: &GraphGeneration,
    vertex: &ProjectGraphVertex,
) -> Option<String> {
    if !graph_embeds(generation) || is_meta_vertex(vertex) {
        return None;
    }
    let policy = &generation.schema.index_policy;
    if policy.retrieval_excluded_types.contains(&vertex.type_name) {
        return None;
    }
    let definition = generation.schema.vertex_types.get(&vertex.type_name)?;
    let mut text = vertex.label.clone();
    let mut embedded_any = false;
    // Schema order, not vertex order: the definition map is the author's
    // ordering and is what makes the composition deterministic across
    // vertices of one type.
    for (name, term) in &definition.properties {
        if !property_annotations(term).embed {
            continue;
        }
        let Some(value) = vertex.properties.get(name) else {
            continue;
        };
        let value = property_value_string(value);
        if value.trim().is_empty() {
            continue;
        }
        text.push('\n');
        text.push_str(&value);
        embedded_any = true;
    }
    embedded_any.then_some(text)
}

/// Every embed-eligible vertex of one generation, in vertex-id order.
pub fn graph_embed_projections(generation: &GraphGeneration) -> Vec<GraphVertexEmbedProjection> {
    if !graph_embeds(generation) {
        return Vec::new();
    }
    generation
        .vertices
        .values()
        .filter_map(|vertex| {
            vertex_embed_text(generation, vertex).map(|text| GraphVertexEmbedProjection {
                vertex_id: vertex.id.clone(),
                vertex_type: vertex.type_name.clone(),
                text,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GraphAuthority, GraphDescriptor, GraphKey, GraphSchema, GraphScope, GraphSource,
        RetentionPolicy, VertexTypeDefinition,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    fn generation(
        properties: BTreeMap<String, serde_json::Value>,
        embeddings_enabled: bool,
        source: GraphSource,
    ) -> GraphGeneration {
        let mut schema = GraphSchema {
            version: 1,
            namespace: "test".into(),
            vertex_types: BTreeMap::from([(
                "record:Decision".into(),
                VertexTypeDefinition {
                    required: Vec::new(),
                    properties,
                    hints: Vec::new(),
                },
            )]),
            edge_types: Vec::new(),
            index_policy: Default::default(),
        };
        schema.index_policy.embeddings_enabled = embeddings_enabled;
        let vertex = ProjectGraphVertex {
            id: "decision/one".into(),
            type_name: "record:Decision".into(),
            label: "Delete then insert".into(),
            properties: BTreeMap::from([
                (
                    "question".into(),
                    json!("Should the writer replace rows by delete-then-insert?"),
                ),
                ("rationale".into(), json!("Upserts strand vanished rows.")),
                ("status".into(), json!("accepted")),
                ("secret".into(), json!("never embed me")),
            ]),
        };
        GraphGeneration {
            key: GraphKey {
                scope_id: "p_0000".into(),
                graph_id: "pg".into(),
                source,
            },
            descriptor: GraphDescriptor {
                descriptor_version: 1,
                scope: GraphScope::Project,
                graph_id: "pg".into(),
                authority: GraphAuthority::Project,
                schema_id: "schema".into(),
                schema_version: 1,
                projection_version: None,
                source_connector: None,
                retention_policy: RetentionPolicy::ProjectOwned,
                generation: 1,
            },
            schema,
            vertices: BTreeMap::from([("decision/one".into(), vertex)]),
            edges: Vec::new(),
            fingerprint: "fp".into(),
            source_root: PathBuf::from("/tmp/x"),
            authored_vertex_count: 1,
            authored_edge_count: 0,
        }
    }

    fn annotated() -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([
            (
                "rationale".into(),
                json!({"type": "string", "index": "text", "embed": true}),
            ),
            ("question".into(), json!({"type": "string", "embed": true})),
            ("status".into(), json!({"type": "string", "index": "word"})),
            ("secret".into(), json!({"type": "string"})),
        ])
    }

    #[test]
    fn projection_is_label_plus_embed_props_in_schema_order() {
        let generation = generation(annotated(), true, GraphSource::Committed);
        let projections = graph_embed_projections(&generation);
        assert_eq!(projections.len(), 1);
        // BTreeMap schema order: question before rationale; status (word
        // only) and secret (unannotated) never appear.
        assert_eq!(
            projections[0].text,
            "Delete then insert\nShould the writer replace rows by delete-then-insert?\nUpserts strand vanished rows."
        );
        assert_eq!(projections[0].vertex_type, "record:Decision");
        assert_eq!(
            projections[0].content_hash(),
            graph_vertex_embed_content_hash(&projections[0].text)
        );
        assert_ne!(
            projections[0].content_hash(),
            graph_vertex_embed_content_hash("Delete then insert")
        );
    }

    #[test]
    fn policy_off_embeds_nothing_even_when_annotated() {
        let generation = generation(annotated(), false, GraphSource::Committed);
        assert!(graph_embed_projections(&generation).is_empty());
        assert!(!graph_embeds(&generation));
    }

    #[test]
    fn local_scratch_never_embeds() {
        let generation = generation(annotated(), true, GraphSource::LocalScratch);
        assert!(graph_embed_projections(&generation).is_empty());
    }

    #[test]
    fn excluded_types_and_meta_vertices_never_embed() {
        let mut generation = generation(annotated(), true, GraphSource::Committed);
        generation.vertices.insert(
            "record:Decision".into(),
            ProjectGraphVertex {
                id: "record:Decision".into(),
                type_name: META_VERTEX_TYPE.into(),
                label: "record:Decision".into(),
                properties: BTreeMap::new(),
            },
        );
        assert_eq!(graph_embed_projections(&generation).len(), 1);
        generation.schema.index_policy.retrieval_excluded_types =
            BTreeSet::from(["record:Decision".into()]);
        assert!(graph_embed_projections(&generation).is_empty());
    }

    #[test]
    fn unannotated_or_absent_values_yield_no_projection() {
        // No embed annotations at all: label-only is not a vector.
        let generation = generation(
            BTreeMap::from([("status".into(), json!({"type": "string", "index": "word"}))]),
            true,
            GraphSource::Committed,
        );
        assert!(graph_embed_projections(&generation).is_empty());
        // Annotated, but the vertex carries no value for it.
        let mut generation = generation_with_only(json!({"type": "string", "embed": true}));
        assert!(graph_embed_projections(&generation).is_empty());
        generation
            .vertices
            .get_mut("decision/one")
            .unwrap()
            .properties
            .insert("summary".into(), json!("   "));
        assert!(graph_embed_projections(&generation).is_empty());
        generation
            .vertices
            .get_mut("decision/one")
            .unwrap()
            .properties
            .insert("summary".into(), json!("a real summary"));
        assert_eq!(
            graph_embed_projections(&generation)[0].text,
            "Delete then insert\na real summary"
        );
    }

    fn generation_with_only(summary_term: serde_json::Value) -> GraphGeneration {
        let mut generation = generation(
            BTreeMap::from([("summary".into(), summary_term)]),
            true,
            GraphSource::Committed,
        );
        generation
            .vertices
            .get_mut("decision/one")
            .unwrap()
            .properties
            .clear();
        generation
    }

    #[test]
    fn non_string_values_embed_their_json_form() {
        let mut generation = generation_with_only(json!({"type": "array", "embed": true}));
        generation
            .vertices
            .get_mut("decision/one")
            .unwrap()
            .properties
            .insert("summary".into(), json!(["alpha", "beta"]));
        assert_eq!(
            graph_embed_projections(&generation)[0].text,
            "Delete then insert\n[\"alpha\",\"beta\"]"
        );
    }
}
