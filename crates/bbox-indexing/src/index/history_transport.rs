//! Typed producer history adapter into the certified P3-F builder.
//!
//! This is the third and final authorized input to the one generation
//! construction path. It never writes a generation directly: source facts
//! are lowered to the same canonical document/vector rows as a checkout walk,
//! then handed to `prepare_history_generation` so an activation journal can
//! bind the exact future id before publication.

use std::collections::{BTreeMap, HashMap};

use bbox_chunker::Edge;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_index::index::history_generations::{
    HistoryCommitDocumentV1, HistoryGenerationInputV1, HistoryGenerationOwnerV1,
    HistoryGenerationRecordV1, PreparedHistoryGenerationV1, generation_rows_for_commit,
    live_schema_evidence,
};
use bbox_git_source_store::{GitSourceStore, VerifiedGitHistorySourceV1};
use tantivy::collector::DocSetCollector;
use tantivy::query::{BooleanQuery, Occur, TermQuery};
use tantivy::schema::{IndexRecordOption, Term};

use super::consolidated_history::RepoHistoryIngestGroupV1;
use super::history_materializer::{
    HistoryMaterializerError, HistoryMaterializerResult, prepare_history_generation,
};

const PRODUCER_SOURCE_MARKER: &str = "blackbox.repo-history-generation.producer-transport.v1";

pub struct PreparedTypedHistoryGenerationV1 {
    pub prepared: PreparedHistoryGenerationV1,
    pub source: VerifiedGitHistorySourceV1,
}

/// Lower a complete verified producer snapshot to the canonical P3 rows.
pub fn prepare_typed_history_generation(
    store: &GitSourceStore,
    source: &VerifiedGitHistorySourceV1,
) -> HistoryMaterializerResult<PreparedTypedHistoryGenerationV1> {
    let mut documents = Vec::new();
    let mut vectors = Vec::new();
    store
        .visit_verified_history_commits(source, |row| {
            let (document, vector) =
                generation_rows_for_commit(&row.commit, source.primary_namespace.as_str());
            documents.push(document);
            vectors.push(vector);
            Ok(())
        })
        .map_err(|error| {
            HistoryMaterializerError::new(
                "error.history_transport_source_invalid",
                error.to_string(),
            )
        })?;
    let truncated_message_count = vectors
        .iter()
        .filter(|input| {
            input
                .message
                .ends_with(super::git_history::TRUNCATED_COMMIT_MESSAGE_SUFFIX)
        })
        .count() as u64;
    let (schema_version, schema_fingerprint) = live_schema_evidence()?;
    let prepared = prepare_history_generation(HistoryGenerationInputV1 {
        namespace: source.primary_namespace.clone(),
        owner: HistoryGenerationOwnerV1::Owned {
            repo_history_id: source.repo_history_id.clone(),
        },
        commit_documents: documents,
        vector_inputs: vectors,
        truncated_message_count,
        source_schema_version: schema_version,
        source_schema_fingerprint_sha256: schema_fingerprint,
        source_index_fingerprint_sha256: format!(
            "{PRODUCER_SOURCE_MARKER}:{}:{}",
            source.source_generation_id, source.source_evidence
        ),
    })?;
    Ok(PreparedTypedHistoryGenerationV1 {
        prepared,
        source: source.clone(),
    })
}

/// Re-read the immutable source and derive the same parent/file edges as the
/// checkout-backed consolidated walk.
///
/// `targets_by_project` contains only active code generations admitted to the
/// activation plan. A catalog sibling with no active code selector receives
/// no overlay and therefore no edge publication.
pub fn materialize_typed_history_edges(
    store: &GitSourceStore,
    source: &VerifiedGitHistorySourceV1,
    group: &RepoHistoryIngestGroupV1,
    targets_by_project: &BTreeMap<String, HashMap<String, EntityRef>>,
) -> HistoryMaterializerResult<BTreeMap<String, Vec<Edge>>> {
    if source.repo_history_id != group.repo_history_id
        || source.primary_namespace != group.primary_namespace
    {
        return Err(HistoryMaterializerError::new(
            "error.history_transport_authority_changed",
            "verified Git source no longer matches its repo-history group",
        ));
    }
    let mut edges = targets_by_project
        .keys()
        .map(|project_id| (project_id.clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();
    store
        .visit_verified_history_commits(source, |row| {
            let parents = super::git_history::commit_parent_edges(
                source.primary_namespace.as_str(),
                &row.commit,
            );
            for (project_id, targets) in targets_by_project {
                let Some(root) = group.members.get(project_id) else {
                    anyhow::bail!("history activation target is not a repository member");
                };
                let bucket = edges
                    .get_mut(project_id)
                    .expect("target map seeded an edge bucket");
                bucket.extend(parents.iter().cloned());
                bucket.extend(super::git_history::commit_touched_file_edges(
                    source.primary_namespace.as_str(),
                    &row.commit,
                    root,
                    &row.changed_paths,
                    targets,
                ));
            }
            Ok(())
        })
        .map_err(|error| {
            HistoryMaterializerError::new(
                "error.history_transport_source_invalid",
                error.to_string(),
            )
        })?;
    Ok(edges)
}

/// Prove that Tantivy exposes exactly one complete generation for this
/// namespace: no missing rows, no duplicates, and no force-pushed residue.
///
/// This is intentionally a source-sized recovery probe, not a request-path
/// predicate. It reads only the namespace's committed Tantivy documents and
/// never walks a checkout or the edge-sidecar estate.
pub fn verify_history_commit_view(
    searcher: &tantivy::Searcher,
    fields: bbox_corpus_index::index::FieldHandles,
    generation: &HistoryGenerationRecordV1,
) -> HistoryMaterializerResult<()> {
    let verify = || -> anyhow::Result<()> {
        generation
            .validate()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(
                        fields.repo_id,
                        generation.manifest.body.namespace.as_str(),
                    ),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.doc_type, "commit"),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let addresses = searcher.search(&query, &DocSetCollector)?;
        let mut actual = Vec::with_capacity(addresses.len());
        for address in addresses {
            let document = searcher.doc::<tantivy::TantivyDocument>(address)?;
            actual.push(HistoryCommitDocumentV1 {
                entity_id: bbox_corpus_index::index::first_text(&document, fields.entity_id),
                doc_type: bbox_corpus_index::index::first_text(&document, fields.doc_type),
                chunk_kind: bbox_corpus_index::index::first_text(&document, fields.chunk_kind),
                repo_id: bbox_corpus_index::index::first_text(&document, fields.repo_id),
                commit_sha: bbox_corpus_index::index::first_text(&document, fields.commit_sha),
                content: bbox_corpus_index::index::first_text(&document, fields.content),
                content_hash: bbox_corpus_index::index::first_text(&document, fields.chunk_hash),
                path_tokens: bbox_corpus_index::index::first_text(&document, fields.path_tokens),
                parser_version: bbox_corpus_index::index::first_text(
                    &document,
                    fields.parser_version,
                ),
                commit_author_name: bbox_corpus_index::index::first_text(
                    &document,
                    fields.commit_author_name,
                ),
                commit_author_email: bbox_corpus_index::index::first_text(
                    &document,
                    fields.commit_author_email,
                ),
                session_id: bbox_corpus_index::index::first_text(&document, fields.session_id),
                account: bbox_corpus_index::index::first_text(&document, fields.account),
                role: bbox_corpus_index::index::first_text(&document, fields.role),
                byte_offset: bbox_corpus_index::index::first_u64(&document, fields.byte_offset),
                is_subagent: bbox_corpus_index::index::first_u64(&document, fields.is_subagent),
            });
        }
        actual.sort_by(|left, right| {
            (
                &left.repo_id,
                &left.entity_id,
                &left.commit_sha,
                &left.content_hash,
            )
                .cmp(&(
                    &right.repo_id,
                    &right.entity_id,
                    &right.commit_sha,
                    &right.content_hash,
                ))
        });
        if actual != generation.commit_documents {
            anyhow::bail!(
                "Tantivy namespace does not equal generation {} (expected {} rows, found {})",
                generation.id,
                generation.commit_documents.len(),
                actual.len()
            );
        }
        Ok(())
    };
    verify().map_err(|error| {
        HistoryMaterializerError::new(
            "error.history_transport_commit_view_mismatch",
            error.to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::git::GitCommit;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{CommitNamespace, RepoHistoryId};
    use bbox_git_source::{
        GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1, GitHistoryDescriptorV1,
        GitHistoryManifestEntryV1, GitHistoryManifestPageV1, GitObjectFormatV1, SCHEMA_VERSION,
        encode_history_fragment, history_manifest_sha256,
    };
    use bbox_git_source_store::StoreLimits;
    use sha2::{Digest, Sha256};

    #[test]
    fn typed_source_matches_checkout_generation_and_edge_facts() {
        let directory = tempfile::tempdir().unwrap();
        let store = GitSourceStore::open(
            directory.path().canonicalize().unwrap().join("git-sources"),
            StoreLimits::default(),
        )
        .unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let root = "1".repeat(40);
        let head = "2".repeat(40);
        let fragments = vec![
            GitHistoryCommitFragmentV1 {
                commit_oid: root.clone(),
                fragment_index: 0,
                fragment_count: 1,
                header: Some(GitHistoryCommitHeaderV1 {
                    parent_oids: vec![],
                    author_name: "A".into(),
                    author_email: "a@example.invalid".into(),
                    message: "root".into(),
                }),
                changed_paths: vec!["README.md".into()],
            },
            GitHistoryCommitFragmentV1 {
                commit_oid: head.clone(),
                fragment_index: 0,
                fragment_count: 1,
                header: Some(GitHistoryCommitHeaderV1 {
                    parent_oids: vec![root.clone()],
                    author_name: "B".into(),
                    author_email: "b@example.invalid".into(),
                    message: "head".into(),
                }),
                changed_paths: vec!["src/lib.rs".into()],
            },
        ];
        let records = fragments
            .iter()
            .map(encode_history_fragment)
            .collect::<Vec<_>>();
        let manifest = fragments
            .iter()
            .zip(&records)
            .map(|(fragment, bytes)| GitHistoryManifestEntryV1 {
                commit_oid: fragment.commit_oid.clone(),
                fragment_index: 0,
                encoded_bytes: bytes.len() as u64,
                content_sha256: hex::encode(Sha256::digest(bytes)),
            })
            .collect::<Vec<_>>();
        let descriptor = GitHistoryDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: PublishedScope::try_new("repo-a", ".").unwrap(),
            repo_head: head.clone(),
            object_format: GitObjectFormatV1::Sha1,
            manifest_sha256: history_manifest_sha256(&manifest),
            commit_count: 2,
            fragment_count: 2,
            logical_bytes: manifest.iter().map(|entry| entry.encoded_bytes).sum(),
        };
        let upload = store
            .begin_history_upload("producer-a", &history, &namespace, descriptor)
            .unwrap();
        store
            .put_history_manifest_page(
                "producer-a",
                &upload.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        store
            .complete_history_manifest("producer-a", &upload.upload_id)
            .unwrap();
        for (entry, bytes) in manifest.iter().zip(records) {
            store
                .install_history_record(
                    "producer-a",
                    &upload.upload_id,
                    &entry.content_sha256,
                    entry.encoded_bytes,
                    std::io::Cursor::new(bytes),
                )
                .unwrap();
        }
        let finalized = store
            .finalize_history_upload("producer-a", &upload.upload_id)
            .unwrap();
        let source = store
            .verified_history_source("producer-a", &finalized.source_generation_id)
            .unwrap();
        let typed = prepare_typed_history_generation(&store, &source).unwrap();

        let checkout_facts = vec![
            (
                GitCommit {
                    sha: root.clone(),
                    parent_shas: vec![],
                    author_name: "A".into(),
                    author_email: "a@example.invalid".into(),
                    message: "root".into(),
                },
                vec!["README.md".to_string()],
            ),
            (
                GitCommit {
                    sha: head,
                    parent_shas: vec![root],
                    author_name: "B".into(),
                    author_email: "b@example.invalid".into(),
                    message: "head".into(),
                },
                vec!["src/lib.rs".to_string()],
            ),
        ];
        let mut expected_documents = Vec::new();
        let mut expected_vectors = Vec::new();
        for (commit, _) in &checkout_facts {
            let (document, vector) = generation_rows_for_commit(commit, namespace.as_str());
            expected_documents.push(document);
            expected_vectors.push(vector);
        }
        expected_documents.sort_by(|left, right| {
            (
                &left.repo_id,
                &left.entity_id,
                &left.commit_sha,
                &left.content_hash,
            )
                .cmp(&(
                    &right.repo_id,
                    &right.entity_id,
                    &right.commit_sha,
                    &right.content_hash,
                ))
        });
        expected_vectors.sort_by(|left, right| {
            (&left.entity_id, &left.content_hash).cmp(&(&right.entity_id, &right.content_hash))
        });
        assert_eq!(typed.prepared.record().commit_documents, expected_documents);
        assert_eq!(typed.prepared.record().vector_inputs, expected_vectors);

        let group = RepoHistoryIngestGroupV1 {
            repo_history_id: history,
            primary_namespace: namespace.clone(),
            members: BTreeMap::from([("p_one".to_string(), ".".to_string())]),
        };
        let targets = HashMap::from([
            (
                "README.md".to_string(),
                EntityRef::ProjectFileV2 {
                    project_id: "p_one".into(),
                    snapshot_id: "snapshot-one".into(),
                    rel_path_hash: "a".repeat(64),
                    chunk_hash: "b".repeat(64),
                    occurrence_idx: 0,
                },
            ),
            (
                "src/lib.rs".to_string(),
                EntityRef::ProjectFileV2 {
                    project_id: "p_one".into(),
                    snapshot_id: "snapshot-one".into(),
                    rel_path_hash: "c".repeat(64),
                    chunk_hash: "d".repeat(64),
                    occurrence_idx: 0,
                },
            ),
        ]);
        let actual_edges = materialize_typed_history_edges(
            &store,
            &source,
            &group,
            &BTreeMap::from([("p_one".to_string(), targets.clone())]),
        )
        .unwrap();
        let mut expected_edges = Vec::new();
        for (commit, paths) in &checkout_facts {
            expected_edges.extend(super::super::git_history::commit_parent_edges(
                namespace.as_str(),
                commit,
            ));
            expected_edges.extend(super::super::git_history::commit_touched_file_edges(
                namespace.as_str(),
                commit,
                ".",
                paths,
                &targets,
            ));
        }
        assert_eq!(actual_edges["p_one"], expected_edges);
    }
}
