use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_gaps::gaps::GapStore;
use bbox_gaps::overlay::{
    GapOverlayKey, GapOverlayRecomputeError, GapOverlayRecomputeErrorKind, GapOverlaySnapshot,
    GapOverlayStatus, GapOverlayValue, PublishedGapSnapshot, load_published_snapshot_at_commit,
    recompute_overlay_result,
};
use bbox_indexing::publisher::project_published_scope;
use bbox_knowledge::overlay::ProvisionalMode;

use super::BlackboxServer;

#[derive(Clone)]
pub(crate) struct PublishedGapCacheEntry {
    publisher_root: String,
    publisher_commit: String,
    durable_project: String,
    snapshot: PublishedGapSnapshot,
}

pub(crate) struct SessionGapView {
    pub(crate) gaps: GapStore,
    pub(crate) diagnostics: Vec<String>,
}

impl BlackboxServer {
    /// Recompute the gap twin for one registered checkout. Publisher election,
    /// branch pinning, and invalid-snapshot replacement match knowledge.
    pub(crate) fn refresh_dark_gap_overlay(&self, checkout: &ResolvedCheckoutScope) {
        let _refresh = self.state.gap_overlay_refresh.lock();
        let generation = self
            .state
            .gap_overlays
            .write()
            .begin_refresh(GapOverlayKey {
                published_scope: checkout.published_scope.clone(),
                checkout_id: checkout.checkout_id.clone(),
            });
        let projects = self.state.projects.read().list();
        let prior = self
            .state
            .gap_overlays
            .read()
            .get(&checkout.published_scope, &checkout.checkout_id)
            .cloned();
        let prior_is_valid = prior
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == GapOverlayStatus::Valid);
        let snapshot = match self
            .authorize_publisher_classified(&projects, &checkout.published_scope)
        {
            Ok(publisher) => {
                match stable_gap_overlay(Path::new(&publisher.root), &publisher.commit, checkout) {
                    Ok(snapshot) => snapshot,
                    Err(err)
                        if err.kind == GapOverlayRecomputeErrorKind::Transient
                            && prior_is_valid =>
                    {
                        tracing::warn!(
                                error = %err,
                                checkout = %checkout.checkout_id,
                                scope = ?checkout.published_scope,
                            "gap overlay refresh degraded; preserving prior valid snapshot"
                        );
                        let mut preserved = prior.clone().expect("prior valid snapshot");
                        preserved.diagnostics = vec![format!("refresh degraded: {err:#}")];
                        self.state
                            .gap_overlays
                            .write()
                            .publish_if_latest(generation, preserved);
                        return;
                    }
                    Err(err) => GapOverlaySnapshot::invalid(checkout, format!("{err:#}")),
                }
            }
            Err(err) if err.is_transient() && prior_is_valid => {
                tracing::warn!(
                    error = %err,
                    checkout = %checkout.checkout_id,
                    scope = ?checkout.published_scope,
                    "gap publisher refresh degraded; preserving prior valid snapshot"
                );
                let mut preserved = prior.clone().expect("prior valid snapshot");
                preserved.diagnostics = vec![format!("publisher refresh degraded: {err:#}")];
                self.state
                    .gap_overlays
                    .write()
                    .publish_if_latest(generation, preserved);
                return;
            }
            Err(err) => GapOverlaySnapshot::invalid(checkout, format!("{err:#}")),
        };
        self.state
            .gap_overlays
            .write()
            .publish_if_latest(generation, snapshot);
    }

    /// Materialize pinned published gap records plus the selected provisional
    /// checkout layer. The returned store is detached and read-only.
    pub(crate) fn session_gap_view(
        &self,
        requested_project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<SessionGapView> {
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
            .and_then(|context| {
                projects
                    .iter()
                    .find(|record| record.project_id == context.project_id)
                    .cloned()
            });
        let explicit_managed_scope = requested_record.is_some();
        let managed_paths = projects
            .iter()
            .map(|project| project.canonical_path.as_str())
            .collect::<BTreeSet<_>>();
        let mut gaps = self
            .state
            .gaps
            .read()
            .all()
            .iter()
            .filter(|gap| {
                if self.path_fallback_is_cut() && gap.project.is_some() {
                    return false;
                }
                !gap.project
                    .as_deref()
                    .is_some_and(|project| managed_paths.contains(project))
            })
            .cloned()
            .collect::<Vec<_>>();

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
                    // scope keep their legacy loaded gap view.
                    gaps.extend(
                        self.state
                            .gaps
                            .read()
                            .all()
                            .iter()
                            .filter(|gap| gap.project.as_deref() == Some(&project.canonical_path))
                            .cloned(),
                    );
                }
                None if explicit_managed_scope => anyhow::bail!(
                    "registered project {} has no authoritative published scope",
                    project.canonical_path
                ),
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
            let published = self.cached_published_gap_snapshot(
                Path::new(&publisher.root),
                &publisher.commit,
                &scope,
                &project.canonical_path,
            );
            let published = match published {
                Ok(snapshot) => snapshot,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            let mut scope_gaps = published
                .gaps
                .into_iter()
                .map(|(id, entry)| (id, entry.gap))
                .collect::<BTreeMap<_, _>>();

            match mode {
                ProvisionalMode::Published => {}
                ProvisionalMode::Own => {
                    let Some(own) = session_checkout
                        .as_deref()
                        .filter(|own| own.published_scope == scope)
                    else {
                        gaps.extend(scope_gaps.into_values());
                        continue;
                    };
                    let snapshot = self
                        .state
                        .gap_overlays
                        .read()
                        .get(&scope, &own.checkout_id)
                        .cloned()
                        .with_context(|| {
                            format!(
                                "own checkout gap overlay is missing for scope {scope:?} and checkout {}",
                                own.checkout_id
                            )
                        })?;
                    if snapshot.status != GapOverlayStatus::Valid {
                        anyhow::bail!(
                            "own checkout gap overlay is invalid for scope {scope:?}: {}",
                            snapshot.diagnostics.join("; ")
                        );
                    }
                    diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
                        format!(
                            "checkout {} in gap scope {scope:?}: {diagnostic}",
                            snapshot.key.checkout_id
                        )
                    }));
                    for (id, value) in snapshot.values {
                        match value {
                            GapOverlayValue::Upsert { mut gap, .. } => {
                                gap.project = Some(project.canonical_path.clone());
                                gap.provisional_checkout_id = Some(own.checkout_id.clone());
                                scope_gaps.insert(id, *gap);
                            }
                            GapOverlayValue::Tombstone => {
                                scope_gaps.remove(&id);
                            }
                        }
                    }
                }
                ProvisionalMode::All => {
                    let snapshots = self
                        .state
                        .gap_overlays
                        .read()
                        .snapshots()
                        .filter(|snapshot| snapshot.key.published_scope == scope)
                        .cloned()
                        .collect::<Vec<_>>();
                    for snapshot in snapshots {
                        if snapshot.status != GapOverlayStatus::Valid {
                            diagnostics.push(format!(
                                "checkout {} in gap scope {scope:?}: {}",
                                snapshot.key.checkout_id,
                                snapshot.diagnostics.join("; ")
                            ));
                            continue;
                        }
                        diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
                            format!(
                                "checkout {} in gap scope {scope:?}: {diagnostic}",
                                snapshot.key.checkout_id
                            )
                        }));
                        for (id, value) in snapshot.values {
                            match value {
                                GapOverlayValue::Upsert { mut gap, .. } => {
                                    gap.project = Some(project.canonical_path.clone());
                                    gap.provisional_checkout_id =
                                        Some(snapshot.key.checkout_id.clone());
                                    gaps.push(*gap);
                                }
                                GapOverlayValue::Tombstone => diagnostics.push(format!(
                                    "checkout {} tombstones gap {id}",
                                    snapshot.key.checkout_id
                                )),
                            }
                        }
                    }
                }
            }
            gaps.extend(scope_gaps.into_values());
        }

        Ok(SessionGapView {
            gaps: GapStore::detached_view(gaps),
            diagnostics,
        })
    }

    fn cached_published_gap_snapshot(
        &self,
        publisher_root: &Path,
        publisher_commit: &str,
        scope: &PublishedScope,
        durable_project: &str,
    ) -> Result<PublishedGapSnapshot> {
        let publisher_root = publisher_root.to_string_lossy().into_owned();
        let cached = self
            .state
            .gap_published_cache
            .read()
            .get(scope)
            .filter(|entry| {
                entry.publisher_root == publisher_root
                    && entry.publisher_commit == publisher_commit
                    && entry.durable_project == durable_project
            })
            .cloned();
        if let Some(cached) = cached {
            return Ok(cached.snapshot.clone());
        }

        let snapshot = load_published_snapshot_at_commit(
            Path::new(&publisher_root),
            publisher_commit,
            publisher_commit,
            scope,
            durable_project,
        )?;
        self.state.gap_published_cache.write().insert(
            scope.clone(),
            PublishedGapCacheEntry {
                publisher_root,
                publisher_commit: publisher_commit.to_string(),
                durable_project: durable_project.to_string(),
                snapshot: snapshot.clone(),
            },
        );
        Ok(snapshot)
    }
}

fn stable_gap_overlay(
    publisher_root: &Path,
    published_ref: &str,
    checkout: &ResolvedCheckoutScope,
) -> std::result::Result<GapOverlaySnapshot, GapOverlayRecomputeError> {
    let checkout_root = Path::new(&checkout.checkout_dir);
    if bbox_knowledge::transaction::has_pending_transaction(checkout_root) {
        return Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
            "checkout transaction is pending; provisional gap refresh deferred"
        )));
    }
    let mut candidate = recompute_overlay_result(publisher_root, published_ref, checkout)?;
    for _ in 0..2 {
        if bbox_knowledge::transaction::has_pending_transaction(checkout_root) {
            return Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
                "checkout transaction began during provisional gap refresh"
            )));
        }
        let next = recompute_overlay_result(publisher_root, published_ref, checkout)?;
        if same_gap_snapshot(&candidate, &next)
            && !bbox_knowledge::transaction::has_pending_transaction(checkout_root)
        {
            return Ok(next);
        }
        candidate = next;
    }
    Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
        "checkout state changed repeatedly during provisional gap refresh"
    )))
}

fn same_gap_snapshot(left: &GapOverlaySnapshot, right: &GapOverlaySnapshot) -> bool {
    left.snapshot_id == right.snapshot_id
        && left.status == right.status
        && left.diagnostics == right.diagnostics
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn published_gap_and_authority_reads_share_short_lived_caches() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical temp root");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("create repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Blackbox Test"]);
        std::fs::write(repo.join("README.md"), "seed\n").expect("write seed");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        crate::config::ensure_recorded_repo_id(&repo).expect("record repo id");
        std::fs::create_dir_all(repo.join(".bbox/gaps")).expect("create gaps");
        std::fs::write(
            repo.join(".bbox/gaps/gap-12345678.json"),
            serde_json::to_vec_pretty(&json!({
                "id": "gap-12345678",
                "title": "Cache published gap reads",
                "gap_kind": "tooling",
                "domain": "tests",
                "wanted_capability": "cached published snapshots",
                "dedupe_key": "tooling/tests/cache-published-gap-reads",
                "created_at": "2026-01-01T00:00:00Z"
            }))
            .expect("serialize gap"),
        )
        .expect("write gap");
        git(&repo, &["add", ".bbox"]);
        git(&repo, &["commit", "-q", "-m", "published gap"]);

        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).expect("create state");
        let state = Arc::new(crate::server::SharedState::for_test(&state_dir));
        state
            .projects
            .write()
            .register_path(&repo)
            .expect("register");
        let server = BlackboxServer::new(state.clone());

        let first = server
            .session_gap_view(Some(repo.to_str().expect("utf8 repo")), Some("published"))
            .expect("first gap view");
        assert_eq!(first.gaps.all().len(), 1);
        assert_eq!(state.gap_published_cache.read().len(), 1);
        assert_eq!(state.publisher_authorization_cache.read().len(), 1);

        let second = server
            .session_gap_view(Some(repo.to_str().expect("utf8 repo")), Some("published"))
            .expect("second gap view");
        assert_eq!(second.gaps.all().len(), 1);
        assert_eq!(state.gap_published_cache.read().len(), 1);
        assert_eq!(state.publisher_authorization_cache.read().len(), 1);

        let scope = state
            .gap_published_cache
            .read()
            .keys()
            .next()
            .cloned()
            .expect("cached scope");
        server.invalidate_published_knowledge_cache(&scope);
        assert!(state.gap_published_cache.read().is_empty());
        assert!(state.publisher_authorization_cache.read().is_empty());
    }
}
