use anyhow::Context;

use crate::embed_queue;
use crate::packets::apply_with as apply_packet_with;
use crate::server::BlackboxServer;
use crate::server::routes::rebuild_edge_index_from_shared;

impl BlackboxServer {
    pub(crate) fn sync_knowledge_entry_to_index(&self, entry_id: &str) -> anyhow::Result<()> {
        let logical_ref = crate::index::knowledge_entity_id(entry_id);
        let entry = self.state.kb.read().entry(entry_id).cloned();
        let managed_project = entry
            .as_ref()
            .and_then(|entry| entry.project.as_deref())
            .filter(|project| {
                self.state
                    .projects
                    .read()
                    .list()
                    .iter()
                    .any(|record| record.canonical_path == *project)
            })
            .map(str::to_owned);
        let documents = if let Some(project) = managed_project {
            let expected_entry = entry
                .as_ref()
                .context("managed knowledge entry vanished before index sync")?;
            let expected_entry = serde_json::to_vec(expected_entry)?;
            let documents = self.knowledge_documents_for_project(&project, Some(&logical_ref))?;
            if !documents.iter().any(|document| {
                serde_json::to_vec(&document.entry).is_ok_and(|entry| entry == expected_entry)
            }) {
                anyhow::bail!(
                    "refusing stale knowledge index sync for {logical_ref}: refreshed view does not contain the just-written entry"
                );
            }
            documents
        } else {
            entry
                .map(crate::index::KnowledgeIndexDocument::published)
                .into_iter()
                .collect()
        };
        enqueue_knowledge_documents(&documents);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::ReplaceKnowledgeLogical {
                logical_ref,
                documents,
            });
        Ok(())
    }

    /// Reconcile one logical ref from the overlay/publisher view without
    /// requiring the mutable base store to contain a checkout-authored entry.
    pub(crate) fn sync_knowledge_logical_ref_for_project(
        &self,
        entry_id: &str,
        project: &str,
    ) -> anyhow::Result<()> {
        let logical_ref = crate::index::knowledge_entity_id(entry_id);
        let documents = self.knowledge_documents_for_project(project, Some(&logical_ref))?;
        enqueue_knowledge_documents(&documents);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::ReplaceKnowledgeLogical {
                logical_ref,
                documents,
            });
        Ok(())
    }

    /// Reconcile one complete managed scope when its pinned publisher commit
    /// moves. This removes published ids that disappeared and replaces only
    /// that scope, preserving globals and unrelated projects.
    pub(crate) fn sync_knowledge_scope_to_index(
        &self,
        scope: &bbox_corpus_core::identity::PublishedScope,
        project: &str,
    ) -> anyhow::Result<()> {
        let scope_hash = bbox_knowledge::overlay::published_scope_hash(scope);
        let documents = self
            .knowledge_documents_for_project(project, None)?
            .into_iter()
            .filter(|document| document.scope_hash.as_deref() == Some(scope_hash.as_str()))
            .collect::<Vec<_>>();
        enqueue_knowledge_documents(&documents);
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::ReplaceKnowledgeScope {
                scope_hash,
                documents,
            });
        Ok(())
    }

    pub(crate) fn clear_knowledge_scope_in_index(
        &self,
        scope: &bbox_corpus_core::identity::PublishedScope,
    ) {
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::ReplaceKnowledgeScope {
                scope_hash: bbox_knowledge::overlay::published_scope_hash(scope),
                documents: Vec::new(),
            });
    }

    fn knowledge_documents_for_project(
        &self,
        project: &str,
        logical_ref: Option<&str>,
    ) -> anyhow::Result<Vec<crate::index::KnowledgeIndexDocument>> {
        Ok(self
            .session_knowledge_view(Some(project), Some("all"))?
            .items
            .into_iter()
            .filter(|item| {
                logical_ref.is_none_or(|logical_ref| item.metadata.logical_ref == logical_ref)
            })
            .map(|item| {
                let provisional = item.entity_ref.starts_with("provisional_knowledge:");
                crate::index::KnowledgeIndexDocument {
                    entry: item.entry,
                    entity_id: item.entity_ref,
                    logical_ref: item.metadata.logical_ref,
                    visibility: if provisional {
                        "provisional".into()
                    } else {
                        "published".into()
                    },
                    scope_hash: item
                        .metadata
                        .published_scope
                        .as_ref()
                        .map(bbox_knowledge::overlay::published_scope_hash),
                    checkout_id: item.metadata.checkout_id,
                    snapshot_id: item.metadata.overlay_snapshot_id,
                }
            })
            .collect())
    }

    pub(crate) fn tombstone_knowledge_entry_in_index(&self, entry_id: &str) -> anyhow::Result<()> {
        // The embed tombstone runs unconditionally and first: the tantivy
        // delete is queued on the writer actor (fire-and-forget), and the
        // periodic reindex reconciles tantivy (re-adding only
        // Active|Superseded knowledge docs) but does NOT reconcile vectors —
        // so skipping the embed tombstone on any index-side failure would
        // leak, and bbox_reembed could later revive, a deleted entry in
        // vector search.
        embed_queue::tombstone_knowledge(&crate::index::knowledge_entity_id(entry_id));
        self.state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::ReplaceKnowledgeLogical {
                logical_ref: crate::index::knowledge_entity_id(entry_id),
                documents: Vec::new(),
            });
        Ok(())
    }

    pub(crate) fn rebuild_edge_index_from_stores(&self) -> anyhow::Result<()> {
        // Store mutations only affect structured edges. Re-projecting all
        // Tantivy docs here is a multi-GB path and can stack under concurrent
        // thread updates.
        rebuild_edge_index_from_shared(&self.state, false)
    }

    /// Soft-nag classifier for `bbox_learn`: apply the latest
    /// `content-classification/arc-bound` packet (if one is compiled) to the
    /// entry's content and return a suggestion string when it classifies
    /// arc-bound. System-generated entries (ids prefixed `bb-`, e.g. the
    /// regenerated tool reference) are exempt — their content legitimately
    /// discusses arc-bound patterns in documentation examples. Silent on any
    /// error; this is steering, not enforcement.
    pub(crate) fn arc_bound_warning(&self, id: Option<&str>, content: &str) -> Option<String> {
        if id.is_some_and(|s| s.starts_with("bb-")) {
            return None;
        }
        let packet_store = self.state.packets.read();
        let packets = packet_store.list_all().ok()?;
        let packet = packets
            .into_iter()
            .find(|pk| pk.domain == "content-classification/arc-bound")?;
        let entity = serde_json::json!({ "content": content });
        let prediction = apply_packet_with(&packet, &entity, &*packet_store)?;
        if prediction.classification == "arc_bound" {
            Some(format!(
                "\n\nNote: this content was classified arc-bound by packet {pkt} (rule: {rule}). Active-arc guidance that will not still be correct a year from now usually belongs in `bbox_pin` (scope=work_item/thread/bro/session) rather than `bbox_learn`, where it renders into every unrelated future session's CLAUDE.md. The entry was saved; review and consider pinning instead.",
                pkt = packet.id,
                rule = prediction.rule_id
            ))
        } else {
            None
        }
    }
}

fn enqueue_knowledge_documents(documents: &[crate::index::KnowledgeIndexDocument]) {
    for document in documents
        .iter()
        .filter(|document| crate::index::indexable_knowledge_entry(&document.entry))
    {
        embed_queue::enqueue_knowledge(
            &document.entry,
            &document.entity_id,
            &crate::index::knowledge_chunk_hash(&document.entry),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_knowledge::knowledge::{Approval, Category, KnowledgeEntry, Priority, Scope, Status};
    use bbox_knowledge::overlay::{
        OverlayKey, OverlaySnapshot, OverlayStatus, OverlayValue, provisional_entity_ref,
    };
    use serde_json::json;

    use crate::packets::CompileParams;
    use crate::server::state::SharedState;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn knowledge_entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn write_entry(root: &Path, entry: &KnowledgeEntry) {
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", entry.id)),
            serde_json::to_vec_pretty(entry).unwrap(),
        )
        .unwrap();
    }

    fn managed_overlay_fixture(
        temp: &tempfile::TempDir,
    ) -> (BlackboxServer, PublishedScope, String, String, String) {
        let root = temp.path().canonicalize().unwrap();
        let project = root.join("repo");
        std::fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["config", "user.email", "test@example.com"]);
        git(&project, &["config", "user.name", "Test"]);
        std::fs::write(project.join("README.md"), "seed\n").unwrap();
        git(&project, &["add", "README.md"]);
        git(&project, &["commit", "-q", "-m", "seed"]);
        let repo_id = crate::config::ensure_recorded_repo_id(&project).unwrap();
        write_entry(
            &project,
            &knowledge_entry("shared", "published embedding marker"),
        );
        write_entry(
            &project,
            &knowledge_entry("survivor", "scope survivor embedding marker"),
        );
        git(&project, &["add", ".bbox"]);
        git(&project, &["commit", "-q", "-m", "published knowledge"]);
        let project = project.canonicalize().unwrap();

        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = Arc::new(SharedState::for_test(&state_dir));
        state.projects.write().register_path(&project).unwrap();
        let server = BlackboxServer::new(state.clone());
        let scope = PublishedScope::try_new(repo_id.repo_id, ".").unwrap();
        let checkout_id = "embedding-checkout";
        let provisional = knowledge_entry("shared", "provisional embedding marker");
        let mut values = BTreeMap::new();
        values.insert(
            provisional.id.clone(),
            OverlayValue::Upsert {
                content_hash: crate::index::knowledge_chunk_hash(&provisional),
                entry: Box::new(provisional),
            },
        );
        state.knowledge_overlays.write().publish(OverlaySnapshot {
            snapshot_id: "embedding-snapshot".into(),
            key: OverlayKey {
                published_scope: scope.clone(),
                checkout_id: checkout_id.into(),
            },
            stamp: None,
            status: OverlayStatus::Valid,
            values,
            diagnostics: Vec::new(),
        });
        let project = project.to_string_lossy().into_owned();
        let published_ref = crate::index::knowledge_entity_id("shared");
        let provisional_ref = provisional_entity_ref(&scope, checkout_id, "shared");
        (server, scope, project, published_ref, provisional_ref)
    }

    struct FixedProvider;

    #[async_trait::async_trait]
    impl crate::embed::EmbeddingProvider for FixedProvider {
        async fn embed_batch(
            &self,
            inputs: &[crate::embed::EmbedInput],
            _input_type: crate::embed::EmbedInputType,
        ) -> anyhow::Result<Vec<crate::embed::EmbedOutput>> {
            Ok(inputs
                .iter()
                .map(|_| crate::embed::EmbedOutput::single(vec![1.0, 0.0, 0.0, 0.0]))
                .collect())
        }

        fn dimensions(&self) -> usize {
            4
        }

        fn document_model(&self) -> &str {
            "fixed-test"
        }

        fn endpoint_kind(&self) -> crate::embed::EmbedEndpointKind {
            crate::embed::EmbedEndpointKind::Text
        }

        fn id(&self) -> &str {
            "fixed-test"
        }
    }

    fn install_isolated_knowledge_queue(
        root: &Path,
    ) -> (
        crate::embed::queue::EmbedQueueHandle,
        Arc<crate::vectors::VectorStore>,
    ) {
        let vectors = Arc::new(crate::vectors::VectorStore::open(root).unwrap());
        let queue = crate::embed::queue::EmbedQueueHandle::isolated_for_test(
            "knowledge",
            Arc::new(FixedProvider),
            vectors.clone(),
        );
        crate::embed_queue::install(queue.clone());
        (queue, vectors)
    }

    fn wait_for_vector_entities(
        vectors: &crate::vectors::VectorStore,
        expected: &[&str],
    ) -> Vec<String> {
        for _ in 0..100 {
            let ids = vectors
                .search("knowledge", &[1.0, 0.0, 0.0, 0.0], 32)
                .unwrap()
                .into_iter()
                .map(|hit| hit.id)
                .collect::<Vec<_>>();
            if expected
                .iter()
                .all(|expected| ids.iter().any(|id| id == expected))
            {
                return ids;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        vectors
            .search("knowledge", &[1.0, 0.0, 0.0, 0.0], 32)
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect()
    }

    #[test]
    fn logical_ref_sync_enqueues_exact_published_and_provisional_entities() {
        let temp = tempfile::tempdir().unwrap();
        let (server, _scope, project, published_ref, provisional_ref) =
            managed_overlay_fixture(&temp);
        let (queue, vectors) = install_isolated_knowledge_queue(&temp.path().join("vectors"));

        server
            .sync_knowledge_logical_ref_for_project("shared", &project)
            .unwrap();

        let ids = wait_for_vector_entities(&vectors, &[&published_ref, &provisional_ref]);
        assert!(ids.contains(&published_ref), "{ids:?}");
        assert!(ids.contains(&provisional_ref), "{ids:?}");
        queue.shutdown();
    }

    #[test]
    fn scope_sync_enqueues_every_surviving_exact_entity() {
        let temp = tempfile::tempdir().unwrap();
        let (server, scope, project, published_ref, provisional_ref) =
            managed_overlay_fixture(&temp);
        let survivor_ref = crate::index::knowledge_entity_id("survivor");
        let (queue, vectors) = install_isolated_knowledge_queue(&temp.path().join("vectors"));

        server
            .sync_knowledge_scope_to_index(&scope, &project)
            .unwrap();

        let ids =
            wait_for_vector_entities(&vectors, &[&published_ref, &provisional_ref, &survivor_ref]);
        assert!(ids.contains(&published_ref), "{ids:?}");
        assert!(ids.contains(&provisional_ref), "{ids:?}");
        assert!(ids.contains(&survivor_ref), "{ids:?}");
        queue.shutdown();
    }

    #[test]
    fn arc_bound_warning_fires_on_residue_and_skips_system_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        {
            let store = server.state.packets.read();
            store
                .compile(&CompileParams {
                    domain: "content-classification/arc-bound".into(),
                    classification_lattice: Some(vec!["arc_bound".into(), "standing".into()]),
                    prefix_inference: Some(
                        [
                            ("arc_".into(), "arc_bound".into()),
                            ("standing_".into(), "standing".into()),
                        ]
                        .into(),
                    ),
                    rules: json!([
                        {
                            "id": "arc_named_migration",
                            "antecedent": {
                                "op": "StringContains",
                                "field": "content",
                                "needle": "3-tier migration",
                                "case_insensitive": true
                            },
                            "consequent": "ARC_BOUND"
                        },
                        {
                            "id": "standing_catchall",
                            "classification": "standing",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "STANDING"
                        }
                    ]),
                    scope: Some("global".into()),
                    project: None,
                    source_ids: None,
                    rank_table: None,
                    rank_lookup_key: None,
                    threshold_table: None,
                    threshold_lookup_key: None,
                })
                .unwrap();
        }

        let nag_arc = server.arc_bound_warning(None, "For the 3-tier migration, avoid touching X");
        assert!(
            nag_arc
                .as_deref()
                .is_some_and(|s| s.contains("arc-bound") && s.contains("bbox_pin")),
            "arc-bound content should produce a pin-steering nag: {nag_arc:?}"
        );

        let nag_standing = server.arc_bound_warning(None, "Prefer rustls over openssl");
        assert!(
            nag_standing.is_none(),
            "standing content should not trigger a nag: {nag_standing:?}"
        );

        let nag_system = server.arc_bound_warning(
            Some("bb-tool-reference"),
            "For the 3-tier migration, avoid touching X",
        );
        assert!(
            nag_system.is_none(),
            "system-generated entries must be exempt from the nag: {nag_system:?}"
        );
    }
}
