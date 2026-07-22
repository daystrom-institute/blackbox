use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::publisher::project_published_scope;
use bbox_knowledge::knowledge::{Knowledge, KnowledgeEntry, KnowledgeViewMetadata, Scope};
use bbox_knowledge::overlay::{
    OverlaySnapshot, OverlayStatus, OverlayValue, ProvisionalMode, PublishedKnowledgeSnapshot,
    load_published_snapshot_at_commit_unhydrated, provisional_entity_ref,
};

use super::BlackboxServer;

#[derive(Clone)]
pub(crate) struct PublishedKnowledgeCacheEntry {
    publisher_root: String,
    publisher_commit: String,
    durable_project: String,
    snapshot: PublishedKnowledgeSnapshot,
}

#[derive(Debug, Clone)]
pub(crate) struct KnowledgeViewItem {
    pub(crate) entity_ref: String,
    pub(crate) entry: KnowledgeEntry,
    pub(crate) metadata: KnowledgeViewMetadata,
}

pub(crate) struct SessionKnowledgeView {
    pub(crate) knowledge: Knowledge,
    pub(crate) items: Vec<KnowledgeViewItem>,
    pub(crate) diagnostics: Vec<String>,
}

impl SessionKnowledgeView {
    pub(crate) fn diagnostics_text(&self) -> Option<String> {
        (!self.diagnostics.is_empty()).then(|| {
            format!(
                "provisional visibility degraded:\n- {}",
                self.diagnostics.join("\n- ")
            )
        })
    }

    pub(crate) fn append_diagnostics(&self, output: String) -> String {
        match self.diagnostics_text() {
            Some(diagnostics) => format!("{output}\n{diagnostics}"),
            None => output,
        }
    }
}

impl BlackboxServer {
    pub(crate) fn authoritative_session_checkout(&self) -> Option<Arc<ResolvedCheckoutScope>> {
        self.session_checkout.get().and_then(Clone::clone)
    }

    /// Drop committed-tree snapshots after a caller has already resolved and
    /// validated the current publisher authority.
    pub(crate) fn invalidate_published_snapshot_caches(&self, scope: &PublishedScope) {
        self.state.knowledge_published_cache.write().remove(scope);
        self.state.gap_published_cache.write().remove(scope);
    }

    /// Invalidate one scope's authority decision with generation protection so
    /// an already-running resolution cannot repopulate a stale result.
    pub(crate) fn invalidate_publisher_authority_cache(&self, scope: &PublishedScope) {
        self.state
            .publisher_authorization_cache
            .write()
            .invalidate(scope);
    }

    /// External publisher, registry, or ref movement invalidates both the
    /// authority decision and any snapshots derived from it.
    pub(crate) fn invalidate_published_knowledge_cache(&self, scope: &PublishedScope) {
        self.invalidate_published_snapshot_caches(scope);
        self.invalidate_publisher_authority_cache(scope);
    }

    #[cfg(test)]
    pub(crate) fn set_session_checkout_for_test(
        &self,
        published_scope: PublishedScope,
        checkout_id: String,
        checkout_dir: std::path::PathBuf,
    ) {
        self.session_checkout
            .set(Some(Arc::new(ResolvedCheckoutScope {
                project_id: "test-project".into(),
                published_scope,
                checkout_id,
                checkout_project_dir: checkout_dir.to_string_lossy().into_owned(),
                branch_ref: bbox_corpus_core::git::current_branch(&checkout_dir)
                    .map(|branch| format!("refs/heads/{branch}")),
                checkout_dir: checkout_dir.to_string_lossy().into_owned(),
            })))
            .unwrap();
    }

    /// Resolve one visibility decision and materialize the exact candidate set
    /// shared by list, search, inspection, and render consumers.
    pub(crate) fn session_knowledge_view(
        &self,
        requested_project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<SessionKnowledgeView> {
        let session_checkout = self.authoritative_session_checkout();
        let mode = ProvisionalMode::parse(provisional, session_checkout.is_some())?;
        let projects = self.state.projects.read().list();
        let requested_record = requested_project
            .and_then(|raw| {
                crate::projects::resolve_project_context(
                    raw,
                    &projects,
                    crate::projects::ResolveIntent::Read,
                )
            })
            .and_then(|ctx| {
                projects
                    .iter()
                    .find(|record| record.project_id == ctx.project_id)
                    .cloned()
            });
        let explicit_managed_scope = requested_record.is_some();
        let managed_paths = projects
            .iter()
            .map(|project| project.canonical_path.as_str())
            .collect::<BTreeSet<_>>();
        let mut items = BTreeMap::<String, KnowledgeViewItem>::new();
        for entry in self.state.kb.read().all_entries() {
            if self.path_fallback_is_cut() && entry.scope == Scope::Project {
                continue;
            }
            let is_managed_project = entry
                .project
                .as_deref()
                .is_some_and(|project| managed_paths.contains(project));
            if entry.scope == Scope::Project && is_managed_project {
                continue;
            }
            insert_published_item(&mut items, entry.clone(), None, None);
        }

        let selected_projects = requested_record
            .as_ref()
            .map(|record| vec![record.clone()])
            .unwrap_or_else(|| projects.clone());
        let mut selected_scopes = BTreeMap::<PublishedScope, ProjectRecord>::new();
        let mut diagnostics = Vec::new();
        for project in selected_projects {
            match project_published_scope(&project, crate::config::read_repo_id_inputs) {
                Some(scope) => {
                    selected_scopes.entry(scope).or_insert(project);
                }
                None if !self.path_fallback_is_cut() => {
                    // Inventory-bounded compatibility until the final path
                    // fallback cut: registered projects without a recorded
                    // scope keep their legacy loaded knowledge view.
                    for entry in self.state.kb.read().all_entries().iter().filter(|entry| {
                        entry.scope == Scope::Project
                            && entry.project.as_deref() == Some(&project.canonical_path)
                    }) {
                        insert_published_item(&mut items, entry.clone(), None, None);
                    }
                }
                None if explicit_managed_scope => {
                    anyhow::bail!(
                        "registered project {} has no authoritative published scope",
                        project.canonical_path
                    );
                }
                None => diagnostics.push(format!(
                    "registered project {} has no authoritative published scope",
                    project.canonical_path
                )),
            }
        }

        for (scope, project) in selected_scopes {
            let publisher = match self.authorize_publisher(&projects, &scope) {
                Ok(publisher) => publisher,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            let published = self.cached_published_knowledge_snapshot(
                Path::new(&publisher.root),
                &publisher.commit,
                &scope,
                &project.canonical_path,
            );
            let published = match published {
                Ok(published) => published,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            for published_entry in published.entries.into_values() {
                insert_published_item(
                    &mut items,
                    published_entry.entry,
                    Some(scope.clone()),
                    Some(published_entry.content_hash),
                );
            }

            match mode {
                ProvisionalMode::Published => {}
                ProvisionalMode::Own => {
                    let Some(own) = session_checkout
                        .as_deref()
                        .filter(|own| own.published_scope == scope)
                    else {
                        continue;
                    };
                    let snapshot = self
                        .state
                        .knowledge_overlays
                        .read()
                        .get(&scope, &own.checkout_id)
                        .cloned()
                        .with_context(|| {
                            format!(
                                "own checkout overlay is missing for scope {scope:?} and checkout {}",
                                own.checkout_id
                            )
                        })?;
                    if snapshot.status != OverlayStatus::Valid {
                        anyhow::bail!(
                            "own checkout overlay is invalid for scope {scope:?}: {}",
                            snapshot.diagnostics.join("; ")
                        );
                    }
                    diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
                        format!(
                            "checkout {} in scope {scope:?}: {diagnostic}",
                            snapshot.key.checkout_id
                        )
                    }));
                    apply_own_overlay(&mut items, &snapshot, &project.canonical_path);
                }
                ProvisionalMode::All => {
                    let snapshots = self
                        .state
                        .knowledge_overlays
                        .read()
                        .snapshots()
                        .filter(|snapshot| snapshot.key.published_scope == scope)
                        .cloned()
                        .collect::<Vec<_>>();
                    for snapshot in snapshots {
                        if snapshot.status != OverlayStatus::Valid {
                            diagnostics.push(format!(
                                "checkout {} in scope {scope:?}: {}",
                                snapshot.key.checkout_id,
                                snapshot.diagnostics.join("; ")
                            ));
                            continue;
                        }
                        diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
                            format!(
                                "checkout {} in scope {scope:?}: {diagnostic}",
                                snapshot.key.checkout_id
                            )
                        }));
                        for (entry_id, value) in &snapshot.values {
                            if matches!(value, OverlayValue::Tombstone) {
                                diagnostics.push(format!(
                                    "checkout {} tombstones knowledge:{entry_id}",
                                    snapshot.key.checkout_id
                                ));
                            }
                        }
                        add_overlay_upserts(&mut items, &snapshot, &project.canonical_path);
                    }
                }
            }
        }

        let items = items.into_values().collect::<Vec<_>>();
        let mut metadata = BTreeMap::new();
        let entries = items
            .iter()
            .map(|item| {
                let mut entry = item.entry.clone();
                if item.entity_ref.starts_with("provisional_knowledge:") {
                    entry.id = item.entity_ref.clone();
                }
                metadata.insert(entry.id.clone(), item.metadata.clone());
                entry
            })
            .collect();
        Ok(SessionKnowledgeView {
            knowledge: Knowledge::detached_view(entries, metadata),
            items,
            diagnostics,
        })
    }

    fn cached_published_knowledge_snapshot(
        &self,
        publisher_root: &Path,
        publisher_commit: &str,
        scope: &PublishedScope,
        durable_project: &str,
    ) -> Result<PublishedKnowledgeSnapshot> {
        let publisher_root = publisher_root.to_string_lossy().into_owned();
        let cached = self
            .state
            .knowledge_published_cache
            .read()
            .get(scope)
            .filter(|entry| {
                entry.publisher_root == publisher_root
                    && entry.publisher_commit == publisher_commit
                    && entry.durable_project == durable_project
            })
            .cloned();
        if let Some(cached) = cached {
            let mut snapshot = cached.snapshot.clone();
            hydrate_published_snapshot(&publisher_root, &mut snapshot);
            return Ok(snapshot);
        }

        let snapshot = load_published_snapshot_at_commit_unhydrated(
            Path::new(&publisher_root),
            publisher_commit,
            publisher_commit,
            scope,
            durable_project,
        )?;
        self.state.knowledge_published_cache.write().insert(
            scope.clone(),
            PublishedKnowledgeCacheEntry {
                publisher_root: publisher_root.clone(),
                publisher_commit: publisher_commit.to_string(),
                durable_project: durable_project.to_string(),
                snapshot: snapshot.clone(),
            },
        );
        let mut hydrated = snapshot;
        hydrate_published_snapshot(&publisher_root, &mut hydrated);
        Ok(hydrated)
    }
}

fn hydrate_published_snapshot(publisher_root: &str, snapshot: &mut PublishedKnowledgeSnapshot) {
    bbox_knowledge::knowledge::hydrate_repo_recall_stats(
        Path::new(publisher_root),
        snapshot
            .entries
            .values_mut()
            .map(|published| &mut published.entry),
    );
}

fn insert_published_item(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    entry: KnowledgeEntry,
    published_scope: Option<PublishedScope>,
    content_hash: Option<String>,
) {
    let entity_ref = EntityRef::Knowledge {
        id: entry.id.clone(),
    }
    .to_string();
    items.insert(
        entity_ref.clone(),
        KnowledgeViewItem {
            metadata: KnowledgeViewMetadata {
                logical_ref: entity_ref.clone(),
                published_scope,
                checkout_id: None,
                content_hash,
                overlay_snapshot_id: None,
            },
            entity_ref,
            entry,
        },
    );
}

fn apply_own_overlay(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    snapshot: &OverlaySnapshot,
    durable_project: &str,
) {
    for (entry_id, value) in &snapshot.values {
        items.remove(
            &EntityRef::Knowledge {
                id: entry_id.clone(),
            }
            .to_string(),
        );
        if matches!(value, OverlayValue::Upsert { .. }) {
            insert_overlay_item(items, snapshot, entry_id, value, durable_project);
        }
    }
}

fn add_overlay_upserts(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    snapshot: &OverlaySnapshot,
    durable_project: &str,
) {
    for (entry_id, value) in &snapshot.values {
        if matches!(value, OverlayValue::Upsert { .. }) {
            insert_overlay_item(items, snapshot, entry_id, value, durable_project);
        }
    }
}

fn insert_overlay_item(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    snapshot: &OverlaySnapshot,
    entry_id: &str,
    value: &OverlayValue,
    durable_project: &str,
) {
    let OverlayValue::Upsert {
        entry,
        content_hash,
    } = value
    else {
        return;
    };
    let entity_ref = provisional_entity_ref(
        &snapshot.key.published_scope,
        &snapshot.key.checkout_id,
        entry_id,
    );
    let mut entry = (**entry).clone();
    entry.project = Some(durable_project.to_string());
    items.insert(
        entity_ref.clone(),
        KnowledgeViewItem {
            metadata: KnowledgeViewMetadata {
                logical_ref: format!("knowledge:{entry_id}"),
                published_scope: Some(snapshot.key.published_scope.clone()),
                checkout_id: Some(snapshot.key.checkout_id.clone()),
                content_hash: Some(content_hash.clone()),
                overlay_snapshot_id: Some(snapshot.snapshot_id.clone()),
            },
            entity_ref,
            entry,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_knowledge::knowledge::{Approval, Category, KnowledgeListParams, Priority, Status};
    use bbox_knowledge::overlay::{OverlayKey, OverlaySnapshot};
    use std::collections::HashMap;
    use std::process::Command;

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

    fn entry(id: &str, content: &str) -> KnowledgeEntry {
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

    fn snapshot(
        scope: &PublishedScope,
        checkout_id: &str,
        values: BTreeMap<String, OverlayValue>,
    ) -> OverlaySnapshot {
        OverlaySnapshot {
            snapshot_id: format!("snapshot-{checkout_id}"),
            key: OverlayKey {
                published_scope: scope.clone(),
                checkout_id: checkout_id.into(),
            },
            stamp: None,
            status: OverlayStatus::Valid,
            values,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn committed_view_enforces_session_authority_tombstones_and_peer_policy() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        git(&base, &["config", "user.email", "test@example.com"]);
        git(&base, &["config", "user.name", "Test"]);
        std::fs::write(base.join("README.md"), "seed\n").unwrap();
        git(&base, &["add", "README.md"]);
        git(&base, &["commit", "-q", "-m", "seed"]);
        let repo_id = crate::config::ensure_recorded_repo_id(&base).unwrap();
        write_entry(&base, &entry("shared", "PUBLISHED_CONTENT"));
        write_entry(&base, &entry("deleted", "PUBLISHED_DELETE_TARGET"));
        git(&base, &["add", ".bbox"]);
        git(&base, &["commit", "-q", "-m", "published knowledge"]);

        let peer_path = temp.path().join("peer");
        git(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "peer-branch",
                peer_path.to_str().unwrap(),
            ],
        );
        // Dirty publisher bytes must not redefine committed truth.
        write_entry(&base, &entry("shared", "DIRTY_PUBLISHER_CONTENT"));

        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = Arc::new(crate::server::SharedState::for_test(&state_dir));
        state.projects.write().register_path(&base).unwrap();
        let server = BlackboxServer::new(state.clone());
        let scope = PublishedScope {
            repo_id: repo_id.repo_id,
            bbox_root_relpath: ".".into(),
        };

        let own_id = "own-checkout";
        let peer_id = "peer-checkout";
        write_entry(&base, &entry("shared", "OWN_CONTENT"));
        std::fs::remove_file(base.join(".bbox/knowledge/deleted.json")).unwrap();
        let mut peer_values = BTreeMap::new();
        peer_values.insert(
            "shared".into(),
            OverlayValue::Upsert {
                entry: Box::new(entry("shared", "PEER_CONTENT")),
                content_hash: "peer-hash".into(),
            },
        );
        state
            .knowledge_overlays
            .write()
            .publish(snapshot(&scope, peer_id, peer_values));
        state.knowledge_overlays.write().publish(OverlaySnapshot {
            snapshot_id: String::new(),
            key: OverlayKey {
                published_scope: scope.clone(),
                checkout_id: "invalid-peer".into(),
            },
            stamp: None,
            status: OverlayStatus::Invalid,
            values: BTreeMap::new(),
            diagnostics: vec!["malformed entry".into()],
        });
        let own_checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: scope.clone(),
            checkout_id: own_id.into(),
            checkout_dir: base.to_string_lossy().into_owned(),
            checkout_project_dir: base.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/main".into()),
        };
        server
            .session_checkout
            .set(Some(Arc::new(own_checkout.clone())))
            .unwrap();

        let published = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            published.knowledge.entry("shared").unwrap().content,
            "PUBLISHED_CONTENT"
        );
        assert!(published.knowledge.entry("deleted").is_some());
        std::fs::create_dir_all(base.join(".bbox/local")).unwrap();
        std::fs::write(
            base.join(".bbox/local/knowledge-stats.json"),
            r#"{"shared":{"recall_count":7,"last_recalled":"2026-07-21T00:00:00Z"}}"#,
        )
        .unwrap();
        let rehydrated = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            rehydrated.knowledge.entry("shared").unwrap().recall_count,
            7,
            "commit-keyed blob caches must rehydrate mutable recall telemetry"
        );

        let missing = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("own"))
            .err()
            .expect("missing own overlay must fail closed");
        assert!(
            missing
                .to_string()
                .contains("own checkout overlay is missing"),
            "{missing:#}"
        );
        server
            .register_dark_knowledge_checkout(&own_checkout)
            .unwrap();
        server.refresh_dark_knowledge_overlay(&own_checkout);

        // A model-supplied peer checkout path scopes the published project but
        // cannot replace the session's own checkout authority.
        let mut own = server
            .session_knowledge_view(Some(peer_path.to_str().unwrap()), Some("own"))
            .unwrap();
        let own_ref = provisional_entity_ref(&scope, own_id, "shared");
        let peer_ref = provisional_entity_ref(&scope, peer_id, "shared");
        assert_eq!(
            own.knowledge.entry(&own_ref).unwrap().content,
            "OWN_CONTENT"
        );
        assert!(own.knowledge.entry(&peer_ref).is_none());
        assert!(own.knowledge.entry("shared").is_none());
        assert!(own.knowledge.entry("deleted").is_none());
        let listed = own
            .knowledge
            .list(&KnowledgeListParams {
                query: Some("OWN_CONTENT".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(listed.contains(&own_ref), "{listed}");

        let all = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("all"))
            .unwrap();
        assert!(all.knowledge.entry("shared").is_some());
        assert!(all.knowledge.entry(&own_ref).is_some());
        assert!(all.knowledge.entry(&peer_ref).is_some());
        let diagnostics = all.diagnostics_text().unwrap();
        assert!(diagnostics.contains("invalid-peer"), "{diagnostics}");
        assert!(
            diagnostics.contains("tombstones knowledge:deleted"),
            "{diagnostics}"
        );

        std::fs::write(peer_path.join(".bbox/knowledge/shared.json"), "not-json").unwrap();
        let invalid_server = BlackboxServer::new(state);
        let invalid_checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: scope,
            checkout_id: "invalid-peer".into(),
            checkout_dir: peer_path.to_string_lossy().into_owned(),
            checkout_project_dir: peer_path.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/peer-branch".into()),
        };
        invalid_server
            .session_checkout
            .set(Some(Arc::new(invalid_checkout.clone())))
            .unwrap();
        invalid_server
            .register_dark_knowledge_checkout(&invalid_checkout)
            .unwrap();
        invalid_server.refresh_dark_knowledge_overlay(&invalid_checkout);
        let error = invalid_server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("own"))
            .err()
            .expect("invalid own overlay must fail closed");
        assert!(
            error
                .to_string()
                .contains("own checkout overlay is invalid")
        );

        write_entry(&base, &entry("shared", "NEW_PUBLISHED_CONTENT"));
        git(&base, &["add", ".bbox/knowledge"]);
        git(
            &base,
            &["commit", "-q", "-m", "advance published knowledge"],
        );
        server.refresh_dark_knowledge_overlay(&own_checkout);
        server.state.index_writer.flush_blocking().unwrap();
        let refreshed = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            refreshed.knowledge.entry("shared").unwrap().content,
            "NEW_PUBLISHED_CONTENT"
        );
        let all = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("all"))
            .unwrap();
        assert!(
            all.knowledge.entry(&own_ref).is_none(),
            "the matching checkout variant must promote away"
        );
        assert_eq!(
            all.knowledge.entry(&peer_ref).unwrap().content,
            "PEER_CONTENT",
            "publisher advancement must preserve another checkout's variant"
        );

        let published_hits = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("NEW PUBLISHED CONTENT", 10, Some("knowledge"))
            .unwrap();
        assert!(
            published_hits
                .iter()
                .any(|hit| hit.entity_id == crate::index::knowledge_entity_id("shared")),
            "{published_hits:?}"
        );
        let peer_hits = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("PEER CONTENT", 10, Some("knowledge"))
            .unwrap();
        assert!(
            !peer_hits.iter().any(|hit| hit.entity_id == peer_ref),
            "static corpus search must not expose checkout-only knowledge: {peer_hits:?}"
        );
        assert!(all.knowledge.entry(&peer_ref).is_some(), "{peer_hits:?}");
    }

    #[test]
    fn pre_cut_legacy_view_is_visible_and_bounded_by_registered_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        for root in [&first, &second] {
            std::fs::create_dir_all(root).unwrap();
            git(root, &["init", "-q", "-b", "main"]);
            git(root, &["config", "user.email", "test@example.com"]);
            git(root, &["config", "user.name", "Test"]);
            std::fs::write(root.join("README.md"), "seed\n").unwrap();
            git(root, &["add", "README.md"]);
            git(root, &["commit", "-q", "-m", "seed"]);
        }

        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = Arc::new(crate::server::SharedState::for_test(&state_dir));
        let first_record = state.projects.write().register_path(&first).unwrap();
        let second_record = state.projects.write().register_path(&second).unwrap();
        assert!(
            project_published_scope(&first_record, crate::config::read_repo_id_inputs).is_none(),
            "the fixture must not have recorded repo identity"
        );
        assert!(
            project_published_scope(&second_record, crate::config::read_repo_id_inputs).is_none(),
            "the fixture must not have recorded repo identity"
        );

        let mut first_entry = entry("first-legacy", "FIRST_LEGACY_CONTENT");
        first_entry.project = Some(first_record.canonical_path.clone());
        state.kb.write().upsert_generated(first_entry).unwrap();
        let mut second_entry = entry("second-legacy", "SECOND_LEGACY_CONTENT");
        second_entry.project = Some(second_record.canonical_path.clone());
        state.kb.write().upsert_generated(second_entry).unwrap();

        let server = BlackboxServer::new(state);
        let aggregate = server.session_knowledge_view(None, None).unwrap();
        assert!(aggregate.knowledge.entry("first-legacy").is_some());
        assert!(aggregate.knowledge.entry("second-legacy").is_some());
        assert!(
            aggregate.diagnostics.is_empty(),
            "{:?}",
            aggregate.diagnostics
        );

        let explicit = server
            .session_knowledge_view(Some(&first_record.canonical_path), None)
            .unwrap();
        assert!(explicit.knowledge.entry("first-legacy").is_some());
        assert!(
            explicit.knowledge.entry("second-legacy").is_none(),
            "an explicit read must not expose another registered legacy scope"
        );
        assert!(
            explicit.diagnostics.is_empty(),
            "{:?}",
            explicit.diagnostics
        );
    }
}
