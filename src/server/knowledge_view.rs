use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::publisher::{PublisherResolution, elect_publisher, project_published_scope};
use bbox_knowledge::knowledge::{Knowledge, KnowledgeEntry, KnowledgeViewMetadata, Scope};
use bbox_knowledge::overlay::{
    OverlaySnapshot, OverlayStatus, OverlayValue, ProvisionalMode, PublishedKnowledgeSnapshot,
    load_published_snapshot_at_commit, provisional_entity_ref,
};

use super::BlackboxServer;

const PUBLISHED_REF_CACHE_TTL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub(crate) struct PublishedKnowledgeCacheEntry {
    publisher_root: String,
    published_ref: String,
    durable_project: String,
    checked_at: Instant,
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
}

impl BlackboxServer {
    pub(crate) fn authoritative_session_checkout(&self) -> Option<Arc<ResolvedCheckoutScope>> {
        self.session_checkout.get().and_then(Clone::clone)
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
        for project in selected_projects {
            match project_published_scope(&project, crate::config::read_repo_id_inputs) {
                Some(scope) => {
                    selected_scopes.entry(scope).or_insert(project);
                }
                None if explicit_managed_scope => {
                    anyhow::bail!(
                        "registered project {} has no authoritative published scope",
                        project.canonical_path
                    );
                }
                None => {}
            }
        }

        let mut diagnostics = Vec::new();
        for (scope, project) in selected_scopes {
            let publisher_root =
                match elect_publisher(&projects, &scope, crate::config::read_repo_id_inputs) {
                    PublisherResolution::One(root) => root,
                    PublisherResolution::None => {
                        let message = format!("no publisher for scope {scope:?}");
                        if explicit_managed_scope {
                            anyhow::bail!(message);
                        }
                        diagnostics.push(message);
                        continue;
                    }
                    PublisherResolution::Duplicate(paths) => {
                        let message = format!(
                            "duplicate publishers for scope {scope:?}: {}",
                            paths.join(", ")
                        );
                        if explicit_managed_scope {
                            anyhow::bail!(message);
                        }
                        diagnostics.push(message);
                        continue;
                    }
                };
            let pin = self
                .state
                .publisher_refs
                .write()
                .ensure_pinned(&scope, Path::new(&publisher_root))
                .with_context(|| format!("pinning publisher for scope {scope:?}"));
            let pin = match pin {
                Ok(pin) => pin,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            let published = self.cached_published_knowledge_snapshot(
                Path::new(&publisher_root),
                &pin.branch_ref,
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
        published_ref: &str,
        scope: &PublishedScope,
        durable_project: &str,
    ) -> Result<PublishedKnowledgeSnapshot> {
        let publisher_root = publisher_root.to_string_lossy().into_owned();
        let now = Instant::now();
        let cached = self
            .state
            .knowledge_published_cache
            .read()
            .get(scope)
            .filter(|entry| {
                entry.publisher_root == publisher_root
                    && entry.published_ref == published_ref
                    && entry.durable_project == durable_project
            })
            .cloned();
        if let Some(cached) = &cached
            && now.duration_since(cached.checked_at) < PUBLISHED_REF_CACHE_TTL
        {
            return Ok(cached.snapshot.clone());
        }

        let publisher_commit =
            bbox_corpus_core::git::resolve_commit(Path::new(&publisher_root), published_ref)
                .with_context(|| {
                    format!("published ref {published_ref} does not resolve in {publisher_root}")
                })?;
        let snapshot = if let Some(cached) = cached
            && cached.snapshot.publisher_commit == publisher_commit
        {
            cached.snapshot
        } else {
            load_published_snapshot_at_commit(
                Path::new(&publisher_root),
                published_ref,
                &publisher_commit,
                scope,
                durable_project,
            )?
        };
        self.state.knowledge_published_cache.write().insert(
            scope.clone(),
            PublishedKnowledgeCacheEntry {
                publisher_root,
                published_ref: published_ref.to_string(),
                durable_project: durable_project.to_string(),
                checked_at: now,
                snapshot: snapshot.clone(),
            },
        );
        Ok(snapshot)
    }
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
        std::thread::sleep(PUBLISHED_REF_CACHE_TTL + Duration::from_millis(25));
        let refreshed = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            refreshed.knowledge.entry("shared").unwrap().content,
            "NEW_PUBLISHED_CONTENT"
        );
    }
}
