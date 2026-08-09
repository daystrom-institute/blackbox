//! Durable authenticated provenance-import publication.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use bbox_git_source::{MAX_PROVENANCE_DOCUMENT_BYTES, ProvenanceImportStateV1};
use bbox_git_source_store::{
    ProvenanceImportJournalV1, ProvenanceImportStageV1, VerifiedProvenanceImportV1,
};
use sha2::{Digest, Sha256};

use super::SharedState;

const MAX_PREPARED_PROVENANCE_IMPORT_EDGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROVENANCE_OBSERVED_SCAN_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) fn spawn_worker(state: &Arc<SharedState>) -> Result<()> {
    let Some(receiver) = state.git_sources.take_provenance_import_receiver() else {
        return Ok(());
    };
    let weak = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("blackbox-provenance-import".to_string())
        .spawn(move || {
            let mut pending = BTreeSet::new();
            loop {
                match receiver.recv_timeout(std::time::Duration::from_secs(30)) {
                    Ok(generation) => {
                        pending.insert(generation);
                        while let Ok(generation) = receiver.try_recv() {
                            pending.insert(generation);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let store = state.git_sources.store();
                match store.ready_provenance_import_ids() {
                    Ok(ids) => pending.extend(ids),
                    Err(error) => {
                        tracing::warn!(%error, "enumerating ready provenance imports failed")
                    }
                }
                match store.list_provenance_import_journals() {
                    Ok(journals) => pending.extend(
                        journals
                            .into_iter()
                            .filter(|journal| !journal.stage.terminal())
                            .map(|journal| journal.import_generation_id),
                    ),
                    Err(error) => {
                        tracing::warn!(%error, "enumerating provenance import journals failed")
                    }
                }
                for generation in std::mem::take(&mut pending) {
                    if let Err(error) = activate_import(&state, &generation) {
                        tracing::warn!(
                            import_generation = %generation,
                            error = %error,
                            "authenticated provenance import did not converge; background redrive will retry"
                        );
                    }
                }
            }
        })
        .context("spawning provenance import worker")?;
    Ok(())
}

pub(crate) fn activate_import(state: &Arc<SharedState>, import_generation_id: &str) -> Result<()> {
    let store = state.git_sources.store();
    let source = store
        .verified_provenance_import(import_generation_id)
        .context("verifying immutable provenance import")?;
    let producer_auth = state.code_sources.producer_auth();
    let current_project = producer_auth
        .project_transport_grant_for_id(&source.producer_id, &source.scope)
        .map_err(|error| anyhow!(error.code()))?;
    if current_project.as_str() != source.project_id {
        bail!("provenance import project authority changed");
    }
    if let Some(coverage) = state.git_transport_coverage_for_project(&source.project_id)?
        && coverage.transport_governed()
        && !coverage.current()
    {
        bail!(
            "provenance import is retained but cannot publish while the covered repository is {coverage:?}"
        );
    }

    let current = match store.current_ready_provenance_import_id(&source.project_id)? {
        Some(current) => Some(current),
        None => store.repair_current_ready_provenance_import_id(&source.project_id)?,
    };
    if current.as_deref() != Some(import_generation_id) {
        let edge_count = supersede_journal_if_owned(&store, &source)?;
        store.settle_provenance_import(import_generation_id, edge_count)?;
        tracing::info!(
            import_generation = import_generation_id,
            project_id = source.project_id,
            "older authenticated provenance import superseded before activation"
        );
        return Ok(());
    }

    let mut journal = match store.read_provenance_import_journal(&source.project_id)? {
        Some(journal)
            if journal.import_generation_id == import_generation_id
                && journal.stage == ProvenanceImportStageV1::Quarantined =>
        {
            // A quarantined source can reach this arm only after an explicit
            // authenticated re-finalize reopened the exact immutable
            // generation. Replace the terminal attempt with a plan pinned to
            // the current read view; background redrive alone cannot do this.
            prepare_journal(state, &source)?
        }
        Some(journal) if journal.import_generation_id == import_generation_id => journal,
        Some(journal) if !journal.stage.terminal() => {
            bail!("an earlier provenance import is still publishing for this project")
        }
        _ => prepare_journal(state, &source)?,
    };
    if matches!(
        journal.stage,
        ProvenanceImportStageV1::Superseded | ProvenanceImportStageV1::Quarantined
    ) {
        return Ok(());
    }
    if journal.stage == ProvenanceImportStageV1::Committed {
        store.settle_provenance_import(import_generation_id, journal.edge_count)?;
        return Ok(());
    }
    let mut prepared_edge_keys = None;
    if journal.stage == ProvenanceImportStageV1::Prepared {
        store.transition_provenance_import(
            import_generation_id,
            ProvenanceImportStateV1::Importing,
            0,
            None,
        )?;
        let preparation = prepare_documents(state, &source, &journal);
        let prepared = match preparation {
            Ok(prepared) => prepared,
            Err(error) if is_invalid_import(&error) => {
                quarantine(&store, &mut journal, &error)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let edge_keys = prepared.ordered_import_keys();
        let edge_count = edge_keys.len() as u64;
        let edges_dir = super::edge_sidecar_dir(state);
        let additional_bytes = prepared.encoded_edge_bytes()?.saturating_add(edge_count);
        let max_existing_lane_bytes =
            super::ensure_edge_index_rebuild_admitted_at(state, &edges_dir, additional_bytes)?;
        bbox_mcp_tools::mcp_tools::provenance::publish_prepared_provenance_import_bounded(
            prepared,
            &edges_dir,
            max_existing_lane_bytes,
        )?;
        journal.stage = ProvenanceImportStageV1::EdgesPublished;
        journal.edge_count = edge_count;
        journal.edge_keys_sha256 = provenance_edge_key_commitment(&edge_keys);
        journal = store.save_provenance_import_journal(journal)?;
        prepared_edge_keys = Some(edge_keys);
    }
    if journal.stage == ProvenanceImportStageV1::EdgesPublished {
        if store
            .current_ready_provenance_import_id(&source.project_id)?
            .as_deref()
            != Some(import_generation_id)
        {
            journal.stage = ProvenanceImportStageV1::Superseded;
            journal.diagnostic =
                Some("superseded by a newer accepted provenance import".to_string());
            store.save_provenance_import_journal(journal.clone())?;
            store.settle_provenance_import(import_generation_id, journal.edge_count)?;
            return Ok(());
        }
        let edge_keys = match prepared_edge_keys {
            Some(edge_keys) => edge_keys,
            None => prepare_documents(state, &source, &journal)?.ordered_import_keys(),
        };
        if edge_keys.len() as u64 != journal.edge_count
            || provenance_edge_key_commitment(&edge_keys) != journal.edge_keys_sha256
        {
            bail!("prepared provenance edge commitment changed during recovery");
        }
        publish_edge_index_and_verify(state, &edge_keys)?;
        let settled = store.settle_provenance_import(import_generation_id, journal.edge_count)?;
        if settled.state == ProvenanceImportStateV1::Active {
            journal.stage = ProvenanceImportStageV1::Committed;
        } else {
            journal.stage = ProvenanceImportStageV1::Superseded;
            journal.diagnostic = settled.diagnostic.clone();
        }
        journal = store.save_provenance_import_journal(journal)?;
    }
    tracing::info!(
        import_generation = import_generation_id,
        project_id = source.project_id,
        documents = source.document_count,
        edges = journal.edge_count,
        stage = ?journal.stage,
        "authenticated provenance import converged"
    );
    Ok(())
}

fn provenance_edge_key_commitment(edge_keys: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-provenance-import-edge-keys-v1\0");
    for key in edge_keys {
        hasher.update((key.len() as u64).to_be_bytes());
        hasher.update(key.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn published_edge_index_contains(state: &SharedState, edge_keys: &[String]) -> bool {
    let mut missing = edge_keys.iter().cloned().collect::<BTreeSet<_>>();
    if missing.is_empty() {
        return true;
    }
    for edge in state.code_read_view.read().edge_index.all_edges() {
        missing.remove(&bbox_edge_sidecar::edge_sidecar::edge_import_key(edge));
        if missing.is_empty() {
            return true;
        }
    }
    false
}

fn publish_edge_index_and_verify(state: &SharedState, edge_keys: &[String]) -> Result<()> {
    if published_edge_index_contains(state, edge_keys) {
        return Ok(());
    }
    let _publication_guard = state
        .index_writer
        .try_begin_edge_index_rebuild()
        .ok_or_else(|| anyhow!("edge-index publication is busy"))?;
    let edges_dir = super::edge_sidecar_dir(state);
    super::rebuild_edge_index_from_shared_at(state, false, &edges_dir)
        .context("publishing authenticated provenance edges into the read view")?;
    if !published_edge_index_contains(state, edge_keys) {
        let view = state.code_read_view.read();
        bail!(
            "published edge index does not contain the provenance edge commitment (expected {}, loaded {})",
            edge_keys.len(),
            view.edge_index.edge_count()
        );
    }
    Ok(())
}

fn supersede_journal_if_owned(
    store: &bbox_git_source_store::GitSourceStore,
    source: &VerifiedProvenanceImportV1,
) -> Result<u64> {
    let Some(mut journal) = store.read_provenance_import_journal(&source.project_id)? else {
        return Ok(0);
    };
    if journal.import_generation_id != source.import_generation_id || journal.stage.terminal() {
        return Ok(journal
            .import_generation_id
            .eq(&source.import_generation_id)
            .then_some(journal.edge_count)
            .unwrap_or(0));
    }
    journal.stage = ProvenanceImportStageV1::Superseded;
    journal.diagnostic = Some("superseded by a newer accepted provenance import".to_string());
    let edge_count = journal.edge_count;
    store.save_provenance_import_journal(journal)?;
    Ok(edge_count)
}

fn prepare_journal(
    state: &Arc<SharedState>,
    source: &VerifiedProvenanceImportV1,
) -> Result<ProvenanceImportJournalV1> {
    let view = state.code_read_view.read();
    let selector = view
        .active_selectors
        .get(&source.project_id)
        .cloned()
        .ok_or_else(|| anyhow!("provenance import project has no active code generation"))?;
    let journal = ProvenanceImportJournalV1::new_prepared(source, view.catalog_epoch, selector)?;
    drop(view);
    let journal = state
        .git_sources
        .store()
        .save_provenance_import_journal(journal)?;
    Ok(journal)
}

fn prepare_documents(
    state: &Arc<SharedState>,
    source: &VerifiedProvenanceImportV1,
    journal: &ProvenanceImportJournalV1,
) -> Result<bbox_mcp_tools::mcp_tools::provenance::PreparedProvenanceImport> {
    let searcher = state.code_read_view.read().searcher.clone();
    let exact_selectors =
        BTreeMap::from([(source.project_id.clone(), journal.code_selector.clone())]);
    let observed_targets = collect_observed_provenance_targets(state, source, journal)?;
    let membership_cache = parking_lot::Mutex::new(BTreeMap::<String, bool>::new());
    let mut prepared_import =
        bbox_mcp_tools::mcp_tools::provenance::PreparedProvenanceImport::default();
    let mut prepared_bytes = 0_u64;
    state
        .git_sources
        .store()
        .visit_verified_provenance_documents(source, |document| {
            let resolve_legacy = |relative_path: &str, byte_range| {
                state
                    .idx
                    .read()
                    .resolve_project_chunk_for_selector_with_searcher(
                        &source.project_id,
                        &journal.code_selector,
                        relative_path,
                        byte_range,
                        &searcher,
                    )
            };
            let target_is_member = |target: &bbox_corpus_core::entity_ref::EntityRef| {
                let key = target.to_string();
                if let Some(member) = membership_cache.lock().get(&key).copied() {
                    return Ok(member);
                }
                let active = state.idx.read().is_active_code_entity_for_with_searcher(
                    &target.to_string(),
                    &exact_selectors,
                    &searcher,
                );
                // Observed provenance is immutable historical evidence. Its
                // project-file identity can legitimately predate the active
                // collected snapshot or the ProjectFileV2 schema. The boot
                // read view may deliberately defer its multi-GB EdgeIndex
                // rebuild, so historical authority comes from the bounded,
                // per-project observed lane scanned once before this pass.
                let observed = !active && observed_targets.contains(&key);
                let member = active || observed;
                membership_cache.lock().insert(key, member);
                Ok(member)
            };
            let prepared =
                bbox_mcp_tools::mcp_tools::provenance::prepare_authenticated_provenance_import(
                    &source.project_id,
                    &source.import_generation_id,
                    &[(
                        document.note_commit,
                        document.document_sha256,
                        document.document,
                    )],
                    &resolve_legacy,
                    &target_is_member,
                )?;
            prepared_bytes = prepared_bytes
                .checked_add(prepared.encoded_edge_bytes()?)
                .ok_or_else(|| anyhow!("provenance edge count overflow"))?;
            if prepared_bytes > MAX_PREPARED_PROVENANCE_IMPORT_EDGE_BYTES {
                bail!("provenance import edge inventory exceeds the enforced memory limit");
            }
            prepared_import.merge(prepared);
            Ok(())
        })?;
    Ok(prepared_import)
}

fn collect_observed_provenance_targets(
    state: &Arc<SharedState>,
    source: &VerifiedProvenanceImportV1,
    journal: &ProvenanceImportJournalV1,
) -> Result<HashSet<String>> {
    let searcher = state.code_read_view.read().searcher.clone();
    let candidates = parking_lot::Mutex::new(HashSet::<String>::new());
    state
        .git_sources
        .store()
        .visit_verified_provenance_documents(source, |document| {
            let resolve_legacy = |relative_path: &str, byte_range| {
                state
                    .idx
                    .read()
                    .resolve_project_chunk_for_selector_with_searcher(
                        &source.project_id,
                        &journal.code_selector,
                        relative_path,
                        byte_range,
                        &searcher,
                    )
            };
            let collect_target = |target: &bbox_corpus_core::entity_ref::EntityRef| {
                candidates.lock().insert(target.to_string());
                Ok(true)
            };
            bbox_mcp_tools::mcp_tools::provenance::prepare_authenticated_provenance_import(
                &source.project_id,
                &source.import_generation_id,
                &[(
                    document.note_commit,
                    document.document_sha256,
                    document.document,
                )],
                &resolve_legacy,
                &collect_target,
            )?;
            Ok(())
        })?;
    let candidates = candidates.into_inner();
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }

    let mut observed = HashSet::new();
    let edges_dir = super::edge_sidecar_dir(state);
    bbox_edge_sidecar::edge_sidecar::visit_observed_edge_lane(
        &edges_dir,
        &source.project_id,
        MAX_PROVENANCE_OBSERVED_SCAN_BYTES,
        MAX_PROVENANCE_DOCUMENT_BYTES as usize,
        |edge| {
            let key = edge.target.to_string();
            if candidates.contains(&key)
                && observed_provenance_edge_authorizes_target(
                    &edge,
                    &source.project_id,
                    &edge.target,
                )
            {
                observed.insert(key);
            }
            Ok(())
        },
    )?;
    Ok(observed)
}

fn observed_provenance_edge_authorizes_target(
    edge: &bbox_edge_index::edge_index::Edge,
    project_id: &str,
    target: &bbox_corpus_core::entity_ref::EntityRef,
) -> bool {
    &edge.target == target
        && matches!(edge.kind.as_str(), "EDITED_FILE" | "READ_FILE")
        && edge.metadata.get("anchor.project_id").map(String::as_str) == Some(project_id)
        // Observed rows written before the catalog-owner backfill have no
        // typed project_id. Their per-project lane and anchor remain valid
        // historical authority.
        && edge
            .project_id
            .as_deref()
            .is_none_or(|owner| owner == project_id)
        && !edge
            .metadata
            .contains_key("provenance.import_generation_id")
}

fn is_invalid_import(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    [
        "invalid provenance document",
        "commit does not match",
        "invalid v2 provenance target_ref",
        "different central project",
        "not in the pinned project corpus",
        "invalid provenance byte range",
        "invalid repository-relative path",
        "provenance import edge inventory exceeds",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn quarantine(
    store: &bbox_git_source_store::GitSourceStore,
    journal: &mut ProvenanceImportJournalV1,
    error: &anyhow::Error,
) -> Result<()> {
    let diagnostic = format!("{error:#}").chars().take(512).collect::<String>();
    journal.stage = ProvenanceImportStageV1::Quarantined;
    journal.diagnostic = Some(diagnostic.clone());
    store.save_provenance_import_journal(journal.clone())?;
    store.transition_provenance_import(
        &journal.import_generation_id,
        ProvenanceImportStateV1::Quarantined,
        journal.edge_count,
        Some(&diagnostic),
    )?;
    tracing::warn!(
        import_generation = journal.import_generation_id,
        project_id = journal.project_id,
        error = %error,
        "authenticated provenance import quarantined"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_corpus_core::entity_ref::EntityRef;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CommitNamespace, CorpusProject, ProjectId, ProjectScope,
        RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization,
        RepoHistoryRecord,
    };
    use bbox_edge_index::edge_index::Edge;
    use bbox_git_source::{
        ProvenanceImportDescriptorV1, ProvenanceImportManifestEntryV1,
        ProvenanceImportManifestPageV1, SCHEMA_VERSION, provenance_manifest_sha256,
    };
    use tantivy::TantivyDocument;

    use super::*;
    use crate::server::CodeReadView;
    use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};

    #[test]
    fn historical_target_requires_matching_observed_project_edge() {
        let project_id = "project-one";
        let target = EntityRef::ProjectFile {
            project_id: project_id.into(),
            rel_path_hash: "path".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project_id.into());
        let observed = Edge {
            source: EntityRef::Transcript {
                provider: "test".into(),
                session_id: "session".into(),
                line_offset: 1,
                event_idx: 0,
            },
            kind: "READ_FILE".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: metadata.clone(),
            project_id: Some(project_id.into()),
        };
        assert!(observed_provenance_edge_authorizes_target(
            &observed, project_id, &target,
        ));
        assert!(!observed_provenance_edge_authorizes_target(
            &observed,
            "another-project",
            &target,
        ));

        let mut legacy_unstamped = observed;
        legacy_unstamped.project_id = None;
        assert!(observed_provenance_edge_authorizes_target(
            &legacy_unstamped,
            project_id,
            &target,
        ));

        let mut imported = legacy_unstamped;
        imported.metadata.insert(
            "provenance.import_generation_id".into(),
            "pis_fixture".into(),
        );
        assert!(!observed_provenance_edge_authorizes_target(
            &imported, project_id, &target,
        ));
    }

    fn state_with_active_project(
        root: &std::path::Path,
    ) -> (Arc<SharedState>, PublishedScope, String, String) {
        let scope = PublishedScope::try_new("provenance-repo", ".").unwrap();
        let project_id = "p_00000000000000000000000000000001".to_string();
        let producer_id = "producer-a".to_string();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let repo_history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        catalog.repo_histories.insert(
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: repo_history_id.clone(),
                membership_generation: 0,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("provenance-repo").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("provenance-repo").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        let parsed_project = ProjectId::parse(project_id.clone()).unwrap();
        catalog.projects.insert(
            parsed_project.clone(),
            CorpusProject {
                project_id: parsed_project,
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Provenance fixture".into(),
                created_at: "2026-08-08T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: Some(repo_history_id),
                languages: BTreeSet::new(),
            },
        );
        catalog.validate().unwrap();
        let catalog_root = root.join("catalog");
        std::fs::create_dir_all(&catalog_root).unwrap();
        let catalog_path = catalog_root.join("projects.json");
        let catalog_store =
            bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
                &catalog_path,
            )
            .unwrap();
        let epoch = catalog_store.snapshot().unwrap().epoch();
        let projects = catalog.projects.clone();
        let repo_histories = catalog.repo_histories.clone();
        catalog_store
            .transact(epoch, |candidate, _| {
                candidate.projects = projects;
                candidate.repo_histories = repo_histories;
                Ok(())
            })
            .unwrap();
        let state = Arc::new(SharedState::for_test_catalog(root, &catalog_path));
        let token = bro_rpc::ServiceToken::parse("9".repeat(64)).unwrap();
        let catalog = catalog_store.snapshot().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: producer_id.clone(),
                        projects: BTreeMap::from([(scope.clone(), project_id.clone())]),
                    },
                )],
                catalog.catalog(),
            )));

        let selector = "collected:provenance-project:generation-one".to_string();
        let fields = state.idx.read().field_handles();
        let mut document = TantivyDocument::default();
        document.add_text(fields.code_source_selector, &selector);
        document.add_text(fields.relative_path, "src/lib.rs");
        document.add_u64(fields.byte_offset, 0);
        document.add_u64(fields.byte_end, 20);
        document.add_text(
            fields.entity_id,
            format!(
                "project_file_v2:{project_id}:snapshot:path:{}:0",
                "a".repeat(64)
            ),
        );
        let mut writer = state.idx.read().index_handle().writer(50_000_000).unwrap();
        writer.add_document(document).unwrap();
        writer.commit().unwrap();
        state.idx.read().reader_reload_for_test();
        let prior = state.code_read_view.read().clone();
        *state.code_read_view.write() = Arc::new(CodeReadView {
            active_selectors: BTreeMap::from([(project_id.clone(), selector)]),
            searcher: state.idx.read().searcher(),
            edge_index: prior.edge_index.clone(),
            catalog_epoch: 1,
            git_overlays: BTreeMap::new(),
        });
        (state, scope, producer_id, project_id)
    }

    fn install_import(
        state: &Arc<SharedState>,
        scope: PublishedScope,
        producer_id: &str,
        project_id: &str,
        commit: &str,
        document: &str,
    ) -> (VerifiedProvenanceImportV1, String) {
        let manifest = vec![ProvenanceImportManifestEntryV1 {
            note_commit: commit.to_string(),
            document_ordinal: 0,
            encoded_bytes: document.len() as u64,
            document_sha256: bbox_provenance::document_sha256(document),
        }];
        let descriptor = ProvenanceImportDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope,
            notes_ref: "refs/notes/bbox/provenance".into(),
            notes_tip: "2".repeat(40),
            manifest_sha256: provenance_manifest_sha256(&manifest),
            document_count: 1,
            logical_bytes: document.len() as u64,
        };
        let store = state.git_sources.store();
        let begun = store
            .begin_provenance_import(producer_id, project_id, descriptor)
            .unwrap();
        store
            .put_provenance_manifest_page(
                producer_id,
                &begun.upload_id,
                0,
                &ProvenanceImportManifestPageV1 { entries: manifest },
            )
            .unwrap();
        store
            .complete_provenance_manifest(producer_id, &begun.upload_id)
            .unwrap();
        let hash = bbox_provenance::document_sha256(document);
        store
            .install_provenance_document(
                producer_id,
                &begun.upload_id,
                &hash,
                document.len() as u64,
                document.as_bytes(),
            )
            .unwrap();
        let finalized = store
            .finalize_provenance_import(producer_id, &begun.upload_id)
            .unwrap();
        (
            store
                .verified_provenance_import(&finalized.import_generation_id)
                .unwrap(),
            begun.upload_id,
        )
    }

    #[test]
    fn sidecar_ahead_recovery_commits_once_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, scope, producer_id, project_id) = state_with_active_project(&root);
        let commit = "1".repeat(40);
        let document = serde_json::json!({
            "schema_version": 1,
            "commit": commit,
            "produced_by": {},
            "tool_calls": [{
                "tool": "Read",
                "source_ref": "transcript:test:session:1:0",
                "file": "src/lib.rs",
                "byte_range": [4, 8]
            }],
            "knowledge_writes": [{"id":"ignored","kind":"remember"}]
        })
        .to_string();
        let manifest = vec![ProvenanceImportManifestEntryV1 {
            note_commit: commit,
            document_ordinal: 0,
            encoded_bytes: document.len() as u64,
            document_sha256: bbox_provenance::document_sha256(&document),
        }];
        let descriptor = ProvenanceImportDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope,
            notes_ref: "refs/notes/bbox/provenance".into(),
            notes_tip: "2".repeat(40),
            manifest_sha256: provenance_manifest_sha256(&manifest),
            document_count: 1,
            logical_bytes: document.len() as u64,
        };
        let store = state.git_sources.store();
        let begun = store
            .begin_provenance_import(&producer_id, &project_id, descriptor)
            .unwrap();
        store
            .put_provenance_manifest_page(
                &producer_id,
                &begun.upload_id,
                0,
                &ProvenanceImportManifestPageV1 { entries: manifest },
            )
            .unwrap();
        store
            .complete_provenance_manifest(&producer_id, &begun.upload_id)
            .unwrap();
        let hash = bbox_provenance::document_sha256(&document);
        store
            .install_provenance_document(
                &producer_id,
                &begun.upload_id,
                &hash,
                document.len() as u64,
                document.as_bytes(),
            )
            .unwrap();
        let finalized = store
            .finalize_provenance_import(&producer_id, &begun.upload_id)
            .unwrap();
        let source = store
            .verified_provenance_import(&finalized.import_generation_id)
            .unwrap();
        let mut journal = prepare_journal(&state, &source).unwrap();
        store
            .transition_provenance_import(
                &source.import_generation_id,
                ProvenanceImportStateV1::Importing,
                0,
                None,
            )
            .unwrap();
        let prepared = prepare_documents(&state, &source, &journal).unwrap();
        let edge_keys = prepared.ordered_import_keys();
        assert_eq!(prepared.edge_count(), 1);
        journal.edge_count = edge_keys.len() as u64;
        journal.edge_keys_sha256 = provenance_edge_key_commitment(&edge_keys);
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &state.idx.read().reindex_config().projects_path,
        );
        bbox_mcp_tools::mcp_tools::provenance::publish_prepared_provenance_import(
            prepared, &edges_dir,
        )
        .unwrap();
        let durable_edge = serde_json::from_str::<bbox_edge_sidecar::edge_sidecar::Edge>(
            std::fs::read_to_string(
                edges_dir
                    .join("explicit")
                    .join(format!("{project_id}.jsonl")),
            )
            .unwrap()
            .trim(),
        )
        .unwrap();
        assert_eq!(
            bbox_edge_sidecar::edge_sidecar::edge_import_key(&durable_edge),
            edge_keys[0]
        );
        journal.stage = ProvenanceImportStageV1::EdgesPublished;
        store
            .save_provenance_import_journal(journal.clone())
            .unwrap();

        activate_import(&state, &source.import_generation_id).unwrap();
        activate_import(&state, &source.import_generation_id).unwrap();
        crate::server::rebuild_edge_index_from_shared(&state, false).unwrap();
        assert!(
            published_edge_index_contains(&state, &edge_keys),
            "an ordinary graph rebuild must retain authenticated provenance edges"
        );
        let status = store
            .provenance_import_status(&producer_id, &source.import_generation_id)
            .unwrap();
        assert_eq!(status.state, ProvenanceImportStateV1::Active);
        assert_eq!(status.edges_imported, 1);
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &state.idx.read().reindex_config().projects_path,
        );
        let sidecar = std::fs::read_to_string(
            edges_dir
                .join("explicit")
                .join(format!("{project_id}.jsonl")),
        )
        .unwrap();
        assert_eq!(sidecar.lines().count(), 1);
        let journal = store
            .read_provenance_import_journal(&project_id)
            .unwrap()
            .unwrap();
        assert_eq!(journal.stage, ProvenanceImportStageV1::Committed);
    }

    #[test]
    fn cross_project_v2_target_is_quarantined_without_sidecar_publication() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, scope, producer_id, project_id) = state_with_active_project(&root);
        let commit = "3".repeat(40);
        let document = serde_json::json!({
            "schema_version": 2,
            "commit": commit,
            "part": {
                "document_id": "d".repeat(64),
                "part_index": 0,
                "part_count": 1
            },
            "produced_by": {},
            "tool_calls": [{
                "tool": "Edit",
                "source_ref": "transcript:test:session:1:0",
                "target_ref": format!(
                    "project_file_v2:other-project:snapshot:path:{}:0",
                    "b".repeat(64)
                ),
                "file": "src/lib.rs"
            }],
            "knowledge_writes": []
        })
        .to_string();
        let (source, _) =
            install_import(&state, scope, &producer_id, &project_id, &commit, &document);
        activate_import(&state, &source.import_generation_id).unwrap();
        let status = state
            .git_sources
            .store()
            .provenance_import_status(&producer_id, &source.import_generation_id)
            .unwrap();
        assert_eq!(status.state, ProvenanceImportStateV1::Quarantined);
        assert!(
            status
                .diagnostic
                .as_deref()
                .is_some_and(|diagnostic| diagnostic.contains("invalid v2 provenance target_ref"))
        );
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &state.idx.read().reindex_config().projects_path,
        );
        assert!(
            !edges_dir
                .join("explicit")
                .join(format!("{project_id}.jsonl"))
                .exists()
        );
    }

    #[test]
    fn explicit_refinalize_retries_quarantine_against_current_observed_edges() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, scope, producer_id, project_id) = state_with_active_project(&root);
        let commit = "4".repeat(40);
        let target = EntityRef::ProjectFile {
            project_id: project_id.clone(),
            rel_path_hash: "b".repeat(64),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 0,
        };
        let document = serde_json::json!({
            "schema_version": 2,
            "commit": commit,
            "part": {
                "document_id": "d".repeat(64),
                "part_index": 0,
                "part_count": 1
            },
            "produced_by": {},
            "tool_calls": [{
                "tool": "Read",
                "source_ref": "transcript:test:session:1:0",
                "target_ref": target.to_string(),
                "file": "src/legacy.rs"
            }],
            "knowledge_writes": []
        })
        .to_string();
        let (source, upload_id) =
            install_import(&state, scope, &producer_id, &project_id, &commit, &document);
        activate_import(&state, &source.import_generation_id).unwrap();
        let store = state.git_sources.store();
        assert_eq!(
            store
                .provenance_import_status(&producer_id, &source.import_generation_id)
                .unwrap()
                .state,
            ProvenanceImportStateV1::Quarantined
        );

        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project_id.clone());
        let observed = Edge {
            source: EntityRef::Transcript {
                provider: "test".into(),
                session_id: "session".into(),
                line_offset: 1,
                event_idx: 0,
            },
            kind: "READ_FILE".into(),
            target,
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata,
            project_id: None,
        };
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &state.idx.read().reindex_config().projects_path,
        );
        bbox_edge_sidecar::edge_sidecar::append_observed_edges(
            &edges_dir,
            &project_id,
            &[observed],
        )
        .unwrap();
        assert_eq!(state.code_read_view.read().edge_index.edge_count(), 0);

        let retried = store
            .finalize_provenance_import(&producer_id, &upload_id)
            .unwrap();
        assert_eq!(retried.import_generation_id, source.import_generation_id);
        assert_eq!(
            store
                .provenance_import_status(&producer_id, &source.import_generation_id)
                .unwrap()
                .state,
            ProvenanceImportStateV1::Ready
        );
        activate_import(&state, &source.import_generation_id).unwrap();
        assert_eq!(
            store
                .provenance_import_status(&producer_id, &source.import_generation_id)
                .unwrap()
                .state,
            ProvenanceImportStateV1::Active
        );
        assert_eq!(
            store
                .read_provenance_import_journal(&project_id)
                .unwrap()
                .unwrap()
                .stage,
            ProvenanceImportStageV1::Committed
        );
    }

    #[test]
    fn oversized_edge_estate_refuses_before_parse_and_remains_recoverable() {
        let mut env = crate::util::TestEnvGuard::new();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, scope, producer_id, project_id) = state_with_active_project(&root);
        let commit = "5".repeat(40);
        let document = serde_json::json!({
            "schema_version": 1,
            "commit": commit,
            "produced_by": {},
            "tool_calls": [{
                "tool": "Read",
                "source_ref": "transcript:test:session:1:0",
                "file": "src/lib.rs",
                "byte_range": [4, 8]
            }],
            "knowledge_writes": []
        })
        .to_string();
        let (source, _) =
            install_import(&state, scope, &producer_id, &project_id, &commit, &document);

        env.set("BLACKBOX_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES", "1");
        let error = activate_import(&state, &source.import_generation_id).unwrap_err();
        assert!(error.to_string().contains("active sidecar input"));
        let status = state
            .git_sources
            .store()
            .provenance_import_status(&producer_id, &source.import_generation_id)
            .unwrap();
        assert_eq!(status.state, ProvenanceImportStateV1::Importing);
        assert_eq!(
            state
                .git_sources
                .store()
                .read_provenance_import_journal(&project_id)
                .unwrap()
                .unwrap()
                .stage,
            ProvenanceImportStageV1::Prepared
        );
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &state.idx.read().reindex_config().projects_path,
        );
        assert!(
            !edges_dir
                .join("explicit")
                .join(format!("{project_id}.jsonl"))
                .exists()
        );

        env.remove("BLACKBOX_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES");
        activate_import(&state, &source.import_generation_id).unwrap();
        assert_eq!(
            state
                .git_sources
                .store()
                .provenance_import_status(&producer_id, &source.import_generation_id)
                .unwrap()
                .state,
            ProvenanceImportStateV1::Active
        );
    }
}
