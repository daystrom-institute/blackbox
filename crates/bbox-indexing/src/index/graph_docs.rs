//! Graph vertex documents for the word index (unified-retrieval design 7.1).
//!
//! One document per AUTHORED vertex of one accepted graph generation, on the
//! published plane only in M9a. The lane replacement here is whole-lane:
//! delete everything under `(project_id, graph_id, graph_source)` and re-emit,
//! because a generation flip may remove vertices that per-document upserts
//! would strand. Property text is gated by the schema's per-property
//! annotations under the graph's `index_policy`; unannotated property values
//! never reach a term dictionary.

use anyhow::Result;
use sha2::{Digest, Sha256};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Term};
use tantivy::{IndexReader, IndexWriter, Searcher, TantivyDocument};

use super::{
    FieldHandles, GRAPH_SOURCE_PUBLISHED, GRAPH_VERTEX_DOC_TYPE, graph_lane_boolean_query,
    graph_lane_stats_for_searcher,
};
use bbox_corpus_core::entity_ref::{EntityRef, PARSER_VERSION};
use bbox_project_graph::{
    FIXED_META_VERTICES, GraphGeneration, META_EDGE_TYPE, META_VERTEX_TYPE, ProjectGraphVertex,
    PropertyIndexMode, property_annotations, property_value_string,
};

/// One vertex prepared for the word index. Plain data so the writer actor
/// op carries no borrow of the graph generation it was built from.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphVertexIndexDocument {
    /// Q6: stamped on every graph vertex document, published and provisional
    /// alike, so the query-side project filter is one exact term and never
    /// parses the ref or consults the catalog mid-query.
    pub project_id: String,
    pub graph_id: String,
    /// Read-plane label (`published` in M9a).
    pub graph_source: String,
    /// Connector plane only in later milestones; published docs carry none.
    pub graph_source_connector: Option<String>,
    /// `ProjectGraphGenerationIdentity.content_hash` of the accepted view the
    /// document was built from.
    pub graph_generation: String,
    pub vertex_id: String,
    pub vertex_type: String,
    pub label: String,
    /// Property values whose schema term annotates `index: "word"`.
    pub word_properties: Vec<String>,
    /// Property values whose schema term annotates `index: "text"`.
    pub text_properties: Vec<String>,
    /// Canonical `EntityRef` string.
    pub entity_id: String,
    /// Logical ref, equal to the entity id on the published plane.
    pub logical_ref: String,
}

/// Whether a projected vertex is schema-as-data rather than an authored fact.
/// The fixed meta floor, the per-type definition vertices, and anything else
/// typed as a meta vertex are never word-indexed: authors search their facts,
/// not the shape of their schema.
pub fn is_meta_vertex(vertex: &ProjectGraphVertex) -> bool {
    FIXED_META_VERTICES.contains(&vertex.id.as_str())
        || vertex.type_name == META_VERTEX_TYPE
        || vertex.type_name == META_EDGE_TYPE
}

/// Build the word-index documents for one published graph generation under
/// its policy. A graph whose `index_policy` disables text retrieval yields
/// ZERO documents, which is what whole-lane replacement turns into an absent
/// lane rather than a filtered-at-query-time one.
pub fn published_graph_vertex_documents(
    project_id: &str,
    generation: &GraphGeneration,
    content_hash: &str,
) -> Vec<GraphVertexIndexDocument> {
    if !generation.schema.index_policy.text_retrieval_enabled {
        return Vec::new();
    }
    let graph_id = generation.key.graph_id.clone();
    let mut documents = Vec::new();
    for vertex in generation.vertices.values() {
        if is_meta_vertex(vertex) {
            continue;
        }
        if generation
            .schema
            .index_policy
            .retrieval_excluded_types
            .contains(&vertex.type_name)
        {
            continue;
        }
        let mut word_properties = Vec::new();
        let mut text_properties = Vec::new();
        if let Some(definition) = generation.schema.vertex_types.get(&vertex.type_name) {
            for (name, value) in &vertex.properties {
                let Some(term) = definition.properties.get(name) else {
                    continue;
                };
                match property_annotations(term).index {
                    PropertyIndexMode::Word => {
                        word_properties.push(property_value_string(value));
                    }
                    PropertyIndexMode::Text => {
                        text_properties.push(property_value_string(value));
                    }
                    PropertyIndexMode::None => {}
                }
            }
        }
        let entity_id = EntityRef::ProjectGraphVertex {
            project_id: project_id.to_string(),
            graph_id: graph_id.clone(),
            vertex_id: vertex.id.clone(),
        }
        .to_string();
        documents.push(GraphVertexIndexDocument {
            project_id: project_id.to_string(),
            graph_id: graph_id.clone(),
            graph_source: GRAPH_SOURCE_PUBLISHED.to_string(),
            graph_source_connector: None,
            graph_generation: content_hash.to_string(),
            vertex_id: vertex.id.clone(),
            vertex_type: vertex.type_name.clone(),
            label: vertex.label.clone(),
            word_properties,
            text_properties,
            logical_ref: entity_id.clone(),
            entity_id,
        });
    }
    documents
}

/// Stable content hash of one indexed vertex body: identity fields plus the
/// property text the policy admitted. Two generations that agree here agree
/// on everything the word index can observe about the vertex.
pub fn graph_vertex_chunk_hash(document: &GraphVertexIndexDocument) -> String {
    let mut hasher = Sha256::new();
    hasher.update(document.entity_id.as_bytes());
    hasher.update(&[0]);
    hasher.update(document.graph_generation.as_bytes());
    hasher.update(&[0]);
    hasher.update(document.label.as_bytes());
    for value in document
        .word_properties
        .iter()
        .chain(&document.text_properties)
    {
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Build the tantivy document. The label is `content`'s first line (the title
/// lane derives the result title from it); `index: text` values follow as the
/// body. `index: word` values ride `path_tokens` under the code tokenizer so
/// identifier-shaped values match exactly the way code symbols do.
pub fn build_graph_vertex_doc(
    source: &GraphVertexIndexDocument,
    f: FieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, GRAPH_VERTEX_DOC_TYPE);
    doc.add_text(f.parser_version, PARSER_VERSION);
    doc.add_text(f.entity_id, &source.entity_id);
    doc.add_text(f.logical_ref, &source.logical_ref);
    doc.add_text(f.project_id, &source.project_id);
    doc.add_text(f.graph_id, &source.graph_id);
    doc.add_text(f.graph_source, &source.graph_source);
    if let Some(connector) = &source.graph_source_connector {
        doc.add_text(f.graph_source_connector, connector);
    }
    doc.add_text(f.graph_generation, &source.graph_generation);
    doc.add_text(f.graph_vertex_type, &source.vertex_type);
    doc.add_text(f.chunk_hash, graph_vertex_chunk_hash(source));
    doc.add_text(f.chunk_kind, GRAPH_VERTEX_DOC_TYPE);
    let mut content = source.label.clone();
    for value in &source.text_properties {
        content.push('\n');
        content.push_str(value);
    }
    doc.add_text(f.content, &content);
    for value in &source.word_properties {
        doc.add_text(f.path_tokens, value);
    }
    doc
}

/// Replace one graph's whole word lane: every document under
/// `(project_id, graph_id, graph_source)` is deleted, then the supplied
/// documents are re-emitted. An empty `documents` purges the lane, which is
/// how a policy-disabled graph and a graph removed from an accepted view both
/// disappear from the index rather than from just the result list.
pub fn apply_graph_lane_replace(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    documents: &[GraphVertexIndexDocument],
) -> Result<()> {
    let Some(key_document) = documents.first() else {
        anyhow::bail!("graph lane replace requires at least one document; purge explicitly");
    };
    let query = graph_lane_boolean_query(
        fields,
        &key_document.project_id,
        Some(&key_document.graph_id),
        &key_document.graph_source,
    );
    writer.delete_query(Box::new(query))?;
    for document in documents {
        writer.add_document(build_graph_vertex_doc(document, fields))?;
    }
    Ok(())
}

/// Purge one graph lane with no replacement. The key is spelled out field by
/// field rather than borrowed from a document because the whole point is that
/// no document of this lane exists anymore.
pub fn apply_graph_lane_purge(
    writer: &mut IndexWriter,
    fields: FieldHandles,
    project_id: &str,
    graph_id: &str,
    graph_source: &str,
) -> Result<()> {
    let query = graph_lane_boolean_query(fields, project_id, Some(graph_id), graph_source);
    writer.delete_query(Box::new(query))?;
    Ok(())
}

/// Read-side lane state for the generation no-op check: the generation stamp
/// of the lane's documents, or `None` when the lane has no documents.
pub fn graph_lane_generation(
    reader: &IndexReader,
    fields: FieldHandles,
    project_id: &str,
    graph_id: &str,
    graph_source: &str,
) -> Result<Option<String>> {
    Ok(graph_lane_stats_for_searcher(
        &reader.searcher(),
        fields,
        project_id,
        graph_id,
        graph_source,
    )?
    .indexed_generation)
}

/// Directly count a lane's documents; test and diagnostic helper that needs
/// no `TranscriptIndex`.
pub fn graph_lane_count(
    searcher: &Searcher,
    fields: FieldHandles,
    project_id: &str,
    graph_id: &str,
    graph_source: &str,
) -> Result<usize> {
    let query = graph_lane_boolean_query(fields, project_id, Some(graph_id), graph_source);
    Ok(searcher.search(&query, &Count)?)
}

/// Collect every stored field of one lane's documents, ordered by tantivy
/// document address. The reindex pass preserves graph lanes this way: like
/// provisional knowledge, graph documents have no durable store the pass
/// walks, so the pass carries them across `delete_all_documents`.
///
/// This is the REINDEX path only, and it is intentionally a full
/// stored-document walk (O(all graph vertices) doc-store reads): a rebuild
/// must re-emit every document verbatim. View installs and lane inventories
/// must not call this; they use the term-dictionary lane enumeration in
/// `bbox_corpus_index` instead.
pub fn collect_graph_lane_documents(
    searcher: &Searcher,
    fields: FieldHandles,
) -> Result<Vec<TantivyDocument>> {
    let query = TermQuery::new(
        Term::from_field_text(fields.doc_type, GRAPH_VERTEX_DOC_TYPE),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    searcher
        .search(&query, &TopDocs::with_limit(count))?
        .into_iter()
        .map(|(_, address)| searcher.doc::<TantivyDocument>(address).map_err(Into::into))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_project_graph::{
        GraphDescriptor, GraphKey, GraphSchema, GraphSource, VertexTypeDefinition,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use tantivy::{Index, IndexReader};

    const PROJECT: &str = "p_00000000000000000000000000000f71";
    const GRAPH: &str = "governance-record";

    fn schema_with(properties: BTreeMap<String, serde_json::Value>) -> GraphSchema {
        GraphSchema {
            version: 1,
            namespace: "test".into(),
            vertex_types: BTreeMap::from([(
                "record:Record".into(),
                VertexTypeDefinition {
                    required: Vec::new(),
                    properties,
                    hints: Vec::new(),
                },
            )]),
            edge_types: Vec::new(),
            index_policy: Default::default(),
        }
    }

    fn generation(schema: GraphSchema, labels: &[&str]) -> GraphGeneration {
        let vertices = labels
            .iter()
            .map(|label| {
                (
                    format!("vertex/{label}"),
                    ProjectGraphVertex {
                        id: format!("vertex/{label}"),
                        type_name: "record:Record".into(),
                        label: (*label).to_string(),
                        properties: BTreeMap::from([
                            ("status".into(), json!("SettlementApproved")),
                            ("summary".into(), json!("quarterly settlement summary")),
                            ("secret_note".into(), json!("unindexed secret value")),
                        ]),
                    },
                )
            })
            .collect();
        GraphGeneration {
            key: GraphKey {
                scope_id: PROJECT.into(),
                graph_id: GRAPH.into(),
                source: GraphSource::Committed,
            },
            descriptor: GraphDescriptor {
                descriptor_version: 1,
                scope: bbox_project_graph::GraphScope::Project,
                graph_id: GRAPH.into(),
                authority: bbox_project_graph::GraphAuthority::Project,
                schema_id: "schema".into(),
                schema_version: 1,
                projection_version: None,
                source_connector: None,
                retention_policy: bbox_project_graph::RetentionPolicy::ProjectOwned,
                generation: 1,
            },
            schema,
            vertices,
            edges: Vec::new(),
            fingerprint: "fingerprint-one".into(),
            source_root: PathBuf::from("/tmp/checkout/.bbox/graphs/governance-record"),
            authored_vertex_count: labels.len(),
            authored_edge_count: 0,
        }
    }

    fn open_index() -> (Index, FieldHandles, IndexReader) {
        let (schema, fields) = super::super::build_schema();
        let index = Index::create_in_ram(schema);
        super::super::register_code_tokenizer(&index);
        let reader = index.reader().unwrap();
        (index, fields, reader)
    }

    fn commit_lane(
        index: &Index,
        fields: FieldHandles,
        documents: &[GraphVertexIndexDocument],
        reader: &IndexReader,
    ) {
        let mut writer = index.writer(50_000_000).unwrap();
        apply_graph_lane_replace(&mut writer, fields, documents).unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        reader.reload().unwrap();
    }

    fn annotated_schema() -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([
            ("status".into(), json!({"type": "string", "index": "word"})),
            ("summary".into(), json!({"type": "string", "index": "text"})),
        ])
    }

    #[test]
    fn builder_emits_one_doc_per_authored_vertex_under_annotations() {
        let generation = generation(schema_with(annotated_schema()), &["Alpha", "Beta"]);
        let documents = published_graph_vertex_documents(PROJECT, &generation, "content-hash-one");
        assert_eq!(documents.len(), 2);
        let alpha = documents
            .iter()
            .find(|document| document.vertex_id == "vertex/Alpha")
            .unwrap();
        assert_eq!(
            alpha.entity_id,
            format!("project_graph_vertex:{PROJECT}:{GRAPH}:vertex/Alpha")
        );
        assert_eq!(alpha.logical_ref, alpha.entity_id);
        assert_eq!(alpha.vertex_type, "record:Record");
        assert_eq!(alpha.graph_generation, "content-hash-one");
        assert_eq!(alpha.project_id, PROJECT);
        assert_eq!(alpha.graph_source, "published");
        assert_eq!(alpha.word_properties, vec!["SettlementApproved"]);
        assert_eq!(alpha.text_properties, vec!["quarterly settlement summary"]);
    }

    #[test]
    fn builder_skips_meta_vertices_and_excluded_types() {
        let mut schema = schema_with(annotated_schema());
        schema.index_policy.retrieval_excluded_types = BTreeSet::from(["record:Hidden".into()]);
        schema.vertex_types.insert(
            "record:Hidden".into(),
            VertexTypeDefinition {
                required: Vec::new(),
                properties: BTreeMap::new(),
                hints: Vec::new(),
            },
        );
        let mut generation = generation(schema, &["Alpha"]);
        generation.vertices.insert(
            "record:Record".into(),
            ProjectGraphVertex {
                id: "record:Record".into(),
                type_name: "meta:VertexType".into(),
                label: "record:Record".into(),
                properties: BTreeMap::new(),
            },
        );
        generation.vertices.insert(
            "meta:VertexType".into(),
            ProjectGraphVertex {
                id: "meta:VertexType".into(),
                type_name: "meta:VertexType".into(),
                label: "meta:VertexType".into(),
                properties: BTreeMap::new(),
            },
        );
        generation.vertices.insert(
            "vertex/gone".into(),
            ProjectGraphVertex {
                id: "vertex/gone".into(),
                type_name: "record:Hidden".into(),
                label: "Hidden Vertex".into(),
                properties: BTreeMap::new(),
            },
        );
        let documents = published_graph_vertex_documents(PROJECT, &generation, "hash");
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].vertex_id, "vertex/Alpha");
    }

    /// Builder-only claim: the committed-index half (a policy flip driving
    /// whole-lane replacement down to an empty, absent lane) is asserted in
    /// writer_actor's graph_lane_replacement_is_generation_gated_and_policy_flip_purges.
    #[test]
    fn policy_disabled_graph_emits_no_builder_documents() {
        let mut schema = schema_with(annotated_schema());
        schema.index_policy.text_retrieval_enabled = false;
        let generation = generation(schema, &["Alpha"]);
        assert!(published_graph_vertex_documents(PROJECT, &generation, "hash").is_empty());
    }

    #[test]
    fn unannotated_property_values_never_reach_the_index() {
        let (index, fields, reader) = open_index();
        // No annotations at all: every property is unindexed.
        let generation = generation(schema_with(BTreeMap::new()), &["Alpha"]);
        let documents = published_graph_vertex_documents(PROJECT, &generation, "hash");
        assert!(documents[0].word_properties.is_empty());
        assert!(documents[0].text_properties.is_empty());
        commit_lane(&index, fields, &documents, &reader);

        let searcher = reader.searcher();
        let term = Term::from_field_text(fields.content, "unindexed");
        let hits = searcher.search(&TermQuery::new(term, IndexRecordOption::Basic), &Count);
        // TEXT field terms are analyzed; query the exact phrase through the
        // stored content of the lane document instead of the dictionary.
        let stored = collect_graph_lane_documents(&searcher, fields).unwrap();
        assert_eq!(stored.len(), 1);
        let content = stored[0]
            .get_first(fields.content)
            .and_then(|value| match value {
                tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert_eq!(content, "Alpha");
        assert!(!content.contains("unindexed secret value"));
        assert_eq!(hits.unwrap(), 0);
    }

    #[test]
    fn generation_flip_removes_vanished_vertices_and_absent_lanes() {
        let (index, fields, reader) = open_index();
        let first = published_graph_vertex_documents(
            PROJECT,
            &generation(schema_with(annotated_schema()), &["Alpha", "Beta"]),
            "generation-one",
        );
        commit_lane(&index, fields, &first, &reader);
        assert_eq!(
            graph_lane_count(
                &reader.searcher(),
                fields,
                PROJECT,
                GRAPH,
                GRAPH_SOURCE_PUBLISHED
            )
            .unwrap(),
            2
        );
        assert_eq!(
            graph_lane_generation(&reader, fields, PROJECT, GRAPH, GRAPH_SOURCE_PUBLISHED)
                .unwrap()
                .as_deref(),
            Some("generation-one")
        );

        // Flip: the new generation dropped Beta entirely.
        let second = published_graph_vertex_documents(
            PROJECT,
            &generation(schema_with(annotated_schema()), &["Alpha"]),
            "generation-two",
        );
        commit_lane(&index, fields, &second, &reader);
        let searcher = reader.searcher();
        assert_eq!(
            graph_lane_count(&searcher, fields, PROJECT, GRAPH, GRAPH_SOURCE_PUBLISHED).unwrap(),
            1
        );
        let remaining = collect_graph_lane_documents(&searcher, fields).unwrap();
        let ids: Vec<&str> = remaining
            .iter()
            .map(|doc| {
                doc.get_first(fields.entity_id)
                    .and_then(|value| match value {
                        tantivy::schema::OwnedValue::Str(value) => Some(value.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            ids,
            vec![format!("project_graph_vertex:{PROJECT}:{GRAPH}:vertex/Alpha").as_str()]
        );

        // Purge: a lane with a policy flip or removal disappears entirely.
        let mut writer = index.writer(50_000_000).unwrap();
        apply_graph_lane_purge(&mut writer, fields, PROJECT, GRAPH, GRAPH_SOURCE_PUBLISHED)
            .unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        reader.reload().unwrap();
        assert_eq!(
            graph_lane_count(
                &reader.searcher(),
                fields,
                PROJECT,
                GRAPH,
                GRAPH_SOURCE_PUBLISHED
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn lane_replace_leaves_foreign_and_provisional_lanes_untouched() {
        let (index, fields, reader) = open_index();
        let published = published_graph_vertex_documents(
            PROJECT,
            &generation(schema_with(annotated_schema()), &["Alpha"]),
            "generation-one",
        );
        let mut provisional = published[0].clone();
        provisional.graph_source = "provisional".into();
        let mut foreign = published[0].clone();
        foreign.project_id = "p_0000000000000000000000000000ffff".into();
        let mut writer = index.writer(50_000_000).unwrap();
        apply_graph_lane_replace(&mut writer, fields, &published).unwrap();
        apply_graph_lane_replace(&mut writer, fields, &[provisional]).unwrap();
        apply_graph_lane_replace(&mut writer, fields, &[foreign]).unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();

        let second = published_graph_vertex_documents(
            PROJECT,
            &generation(schema_with(annotated_schema()), &[]),
            "generation-two",
        );
        assert!(second.is_empty());
        // An empty lane must purge explicitly, never through replace.
        let mut writer = index.writer(50_000_000).unwrap();
        assert!(apply_graph_lane_replace(&mut writer, fields, &second).is_err());
        apply_graph_lane_purge(&mut writer, fields, PROJECT, GRAPH, GRAPH_SOURCE_PUBLISHED)
            .unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
        reader.reload().unwrap();

        let searcher = reader.searcher();
        assert_eq!(
            graph_lane_count(&searcher, fields, PROJECT, GRAPH, "provisional").unwrap(),
            1
        );
        assert_eq!(
            graph_lane_count(
                &searcher,
                fields,
                "p_0000000000000000000000000000ffff",
                GRAPH,
                GRAPH_SOURCE_PUBLISHED
            )
            .unwrap(),
            1
        );
        assert_eq!(
            graph_lane_count(&searcher, fields, PROJECT, GRAPH, GRAPH_SOURCE_PUBLISHED).unwrap(),
            0
        );
    }
}
