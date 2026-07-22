use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bbox_corpus_core::identity::{
    PublishedScope, bbox_root_relpath, read_checkout_id, resolve_recorded_repo_id,
};
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::checkout_registry::{CheckoutRow, discover_checkout_dirs};
use bbox_indexing::publisher::{PublisherResolution, elect_publisher, project_published_scope};
use bbox_knowledge::knowledge::{KnowledgeEntry, Scope};

use super::BlackboxServer;

#[derive(Debug, Default)]
pub(crate) struct KnowledgeCheckoutReconcileReport {
    pub(crate) discovered: usize,
    pub(crate) dropped: usize,
    pub(crate) refreshed: usize,
}

#[derive(Debug, Default)]
pub(crate) struct PathFallbackCutReport {
    pub(crate) cut: bool,
    pub(crate) newly_cut: bool,
    pub(crate) blockers: Vec<String>,
}

pub(crate) struct ExistingKnowledgeMutation {
    pub(crate) id: String,
    pub(crate) carrier: Option<String>,
    pub(crate) seed: Option<KnowledgeEntry>,
    pub(crate) checkout: Option<ResolvedCheckoutScope>,
}

#[derive(Debug)]
pub(crate) struct AuthorizedPublisher {
    pub(crate) root: String,
    pub(crate) branch_ref: String,
    pub(crate) commit: String,
}

impl BlackboxServer {
    /// Resolve the one publisher using committed-HEAD scope claims, then bind
    /// the read to the immutable commit named by the host-local publisher pin.
    /// The pinned commit must carry the same committed scope as the HEAD
    /// candidate that selected the pin.
    pub(crate) fn authorize_publisher(
        &self,
        projects: &[ProjectRecord],
        scope: &PublishedScope,
    ) -> Result<AuthorizedPublisher> {
        let root = match elect_publisher(projects, scope, crate::config::read_repo_id_inputs) {
            PublisherResolution::One(root) => root,
            PublisherResolution::None => anyhow::bail!("no publisher for scope {scope:?}"),
            PublisherResolution::Duplicate(paths) => anyhow::bail!(
                "duplicate publishers for scope {scope:?}: {}",
                paths.join(", ")
            ),
        };
        let pin = self
            .state
            .publisher_refs
            .write()
            .ensure_pinned(scope, Path::new(&root))
            .with_context(|| format!("pinning publisher for scope {scope:?}"))?;
        let commit = bbox_corpus_core::git::resolve_commit(Path::new(&root), &pin.branch_ref)
            .with_context(|| {
                format!(
                    "publisher ref {} does not resolve in {}",
                    pin.branch_ref, root
                )
            })?;
        let project = projects
            .iter()
            .find(|project| project.canonical_path == root)
            .with_context(|| format!("publisher {root} vanished during authority resolution"))?;
        let project_root = Path::new(&project.canonical_path);
        let pinned_inputs = crate::config::read_repo_id_inputs_at_ref(project_root, &commit)
            .with_context(|| {
                format!(
                    "reading publisher authority from {} at {}",
                    pin.branch_ref, commit
                )
            })?;
        let pinned_repo_id = resolve_recorded_repo_id(&pinned_inputs).with_context(|| {
            format!(
                "publisher ref {} has no recorded repo authority at {}",
                pin.branch_ref, commit
            )
        })?;
        let git_root = bbox_corpus_core::git::git_root_for_path(project_root)
            .with_context(|| format!("publisher {root} is not inside a git repository"))?;
        let pinned_scope = PublishedScope {
            repo_id: pinned_repo_id,
            bbox_root_relpath: bbox_root_relpath(&git_root, project_root)
                .with_context(|| format!("publisher {root} is outside its git root"))?,
        };
        if &pinned_scope != scope {
            anyhow::bail!(
                "publisher ref {} resolves to commit {} whose committed project scope does not match {scope:?}",
                pin.branch_ref,
                commit
            );
        }
        Ok(AuthorizedPublisher {
            root,
            branch_ref: pin.branch_ref,
            commit,
        })
    }

    pub(crate) fn path_fallback_is_cut(&self) -> bool {
        self.state
            .path_fallback_cut
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn prepare_existing_knowledge_mutation(
        &self,
        raw_ref: &str,
    ) -> Result<ExistingKnowledgeMutation> {
        let parsed = bbox_corpus_core::entity_ref::EntityRef::parse(raw_ref);
        let (id, provisional_checkout) = match parsed {
            Ok(bbox_corpus_core::entity_ref::EntityRef::Knowledge { id }) => (id, None),
            Ok(bbox_corpus_core::entity_ref::EntityRef::ProvisionalKnowledge {
                checkout_id,
                entry_id,
                ..
            }) => (entry_id, Some(checkout_id)),
            Ok(other) => anyhow::bail!("knowledge mutation requires a knowledge ref, got {other}"),
            Err(_) => (
                raw_ref.trim_start_matches("knowledge:").trim().to_string(),
                None,
            ),
        };
        if id.is_empty() {
            anyhow::bail!("knowledge entry id is required");
        }
        let Some(checkout) = self.authoritative_session_checkout() else {
            if provisional_checkout.is_some() {
                anyhow::bail!("provisional knowledge mutation requires session checkout authority");
            }
            if self.path_fallback_is_cut()
                && self
                    .state
                    .kb
                    .read()
                    .entry(&id)
                    .is_some_and(|entry| entry.scope == Scope::Project)
            {
                anyhow::bail!(
                    "path-scoped project fallback is retired; project knowledge mutation requires session checkout authority"
                );
            }
            return Ok(ExistingKnowledgeMutation {
                id,
                carrier: None,
                seed: None,
                checkout: None,
            });
        };
        if provisional_checkout
            .as_deref()
            .is_some_and(|candidate| candidate != checkout.checkout_id)
        {
            anyhow::bail!("provisional knowledge ref does not belong to the session checkout");
        }
        if bbox_knowledge::transaction::has_pending_transaction(Path::new(&checkout.checkout_dir)) {
            anyhow::bail!(
                "checkout {} has a pending knowledge transaction; restart recovery or finish it before mutating",
                checkout.checkout_id
            );
        }
        self.refresh_dark_knowledge_overlay(&checkout);
        let view =
            self.session_knowledge_view(Some(&checkout.checkout_project_dir), Some("own"))?;
        let item = view
            .items
            .into_iter()
            .find(|item| item.entry.id == id)
            .with_context(|| format!("knowledge entry not visible in session checkout: {id}"))?;
        if item.entry.scope != Scope::Project || item.metadata.published_scope.is_none() {
            return Ok(ExistingKnowledgeMutation {
                id,
                carrier: None,
                seed: None,
                checkout: None,
            });
        }
        if item.metadata.published_scope.as_ref() != Some(&checkout.published_scope) {
            anyhow::bail!("knowledge entry {id} belongs to a different published scope");
        }
        Ok(ExistingKnowledgeMutation {
            id,
            carrier: Some(checkout.checkout_project_dir.clone()),
            seed: Some(item.entry),
            checkout: Some((*checkout).clone()),
        })
    }

    pub(crate) fn finish_existing_knowledge_mutation(
        &self,
        checkout: Option<&ResolvedCheckoutScope>,
    ) {
        if let Some(checkout) = checkout {
            self.refresh_dark_knowledge_overlay(checkout);
        }
    }

    pub(crate) fn recover_abandoned_dark_knowledge_transactions(&self) -> usize {
        self.recover_dark_knowledge_transactions_with(
            bbox_knowledge::transaction::recover_abandoned_pending_transaction,
        )
    }

    fn recover_dark_knowledge_transactions_with(
        &self,
        recover: fn(&Path) -> Result<Option<bbox_knowledge::transaction::RepoTransactionManifest>>,
    ) -> usize {
        let projects = self.state.projects.read().list();
        let mut checkout_dirs = discover_checkout_dirs(&projects)
            .into_iter()
            .map(|path| canonical_or_original(&path))
            .collect::<BTreeSet<_>>();
        checkout_dirs.extend(
            self.state
                .checkout_registry
                .read()
                .rows()
                .iter()
                .map(|row| canonical_or_original(Path::new(&row.checkout_dir))),
        );
        let mut recovered = 0;
        for checkout_dir in checkout_dirs {
            if !bbox_knowledge::transaction::has_pending_transaction(&checkout_dir) {
                continue;
            }
            match recover(&checkout_dir) {
                Ok(Some(_)) => recovered += 1,
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    checkout = %checkout_dir.display(),
                    error = %err,
                    "knowledge transaction recovery failed; checkout remains pending"
                ),
            }
        }
        recovered
    }

    /// Rebuild the host-local checkout census from live authority.
    ///
    /// Persisted rows are hints only. Every row must still exist, carry the
    /// same checkout marker, pass the conservative write resolver, and resolve
    /// to its recorded published scope. Discoverable worktrees are then added
    /// back before every surviving overlay is recomputed.
    pub(crate) fn reconcile_dark_knowledge_checkouts(
        &self,
    ) -> Result<KnowledgeCheckoutReconcileReport> {
        let projects = self.state.projects.read().list();
        let prior_rows = self.state.checkout_registry.read().rows().to_vec();
        let valid = prior_rows
            .iter()
            .filter_map(|row| {
                self.resolve_registered_checkout(row)
                    .map(|_| registry_key(row))
            })
            .collect::<BTreeSet<_>>();
        let dropped = self
            .state
            .checkout_registry
            .write()
            .reconcile(|row| valid.contains(&registry_key(row)))?;

        let mut affected_scopes = BTreeSet::new();
        {
            let mut overlays = self.state.knowledge_overlays.write();
            for row in &dropped {
                if let Some(scope) = row.published_scope() {
                    overlays.remove(&scope, &row.checkout_id);
                    affected_scopes.insert(scope);
                }
            }
        }
        {
            let mut overlays = self.state.gap_overlays.write();
            for row in &dropped {
                if let Some(scope) = row.published_scope() {
                    overlays.remove(&scope, &row.checkout_id);
                }
            }
        }
        if let Some(watcher) = self.state.bbox_watcher.lock().unwrap().as_mut() {
            for row in &dropped {
                let Some(scope) = row.published_scope() else {
                    continue;
                };
                let project_dir =
                    join_repo_relpath(Path::new(&row.checkout_dir), &scope.bbox_root_relpath);
                if let Err(err) = watcher.unwatch_repo_store(&project_dir) {
                    tracing::warn!(
                        checkout = %project_dir.display(),
                        error = %err,
                        "stale checkout row removed but watcher teardown failed"
                    );
                }
            }
        }

        let mut discovered = 0;
        let mut discovered_keys = self
            .state
            .checkout_registry
            .read()
            .rows()
            .iter()
            .map(registry_key)
            .collect::<BTreeSet<_>>();
        for checkout_dir in discover_checkout_dirs(&projects) {
            for project in &projects {
                let Some(expected_scope) = recorded_scope(project) else {
                    continue;
                };
                let checkout_project_dir =
                    join_repo_relpath(&checkout_dir, &expected_scope.bbox_root_relpath);
                if !checkout_project_dir.is_dir() {
                    continue;
                }
                let Some(raw) = checkout_project_dir.to_str() else {
                    continue;
                };
                let Ok(resolution) = self.resolve_project_write(raw) else {
                    continue;
                };
                let Some(checkout) = resolution.checkout_scope else {
                    continue;
                };
                if checkout.published_scope != expected_scope
                    || canonical_or_original(Path::new(&checkout.checkout_dir))
                        != canonical_or_original(&checkout_dir)
                {
                    continue;
                }
                let key = (
                    checkout.checkout_id.clone(),
                    checkout.published_scope.clone(),
                );
                if discovered_keys.insert(key) {
                    discovered += 1;
                }
                self.register_dark_knowledge_checkout(&checkout)?;
            }
        }

        let rows = self.state.checkout_registry.read().rows().to_vec();
        let mut refreshed = 0;
        for row in rows {
            let Some(checkout) = self.resolve_registered_checkout(&row) else {
                continue;
            };
            if bbox_knowledge::transaction::has_pending_transaction(Path::new(
                &checkout.checkout_dir,
            )) {
                continue;
            }
            self.watch_dark_knowledge_checkout(Path::new(&checkout.checkout_project_dir));
            self.refresh_dark_knowledge_overlay(&checkout);
            self.refresh_dark_gap_overlay(&checkout);
            refreshed += 1;
        }
        for scope in affected_scopes {
            self.reconcile_knowledge_scope_index(&scope);
        }

        Ok(KnowledgeCheckoutReconcileReport {
            discovered,
            dropped: dropped.len(),
            refreshed,
        })
    }

    /// Remove registry and overlay state immediately after a successful
    /// checkout teardown. Periodic reconciliation remains the safety net for
    /// removals performed outside the daemon closeout endpoint.
    pub(crate) fn deregister_dark_knowledge_checkout(&self, checkout_id: &str) -> Result<usize> {
        let rows = self
            .state
            .checkout_registry
            .read()
            .rows_for_checkout(checkout_id)
            .cloned()
            .collect::<Vec<_>>();
        let removed = self
            .state
            .checkout_registry
            .write()
            .deregister(checkout_id)?;
        if !removed {
            return Ok(0);
        }
        if let Some(watcher) = self.state.bbox_watcher.lock().unwrap().as_mut() {
            for row in &rows {
                let Some(scope) = row.published_scope() else {
                    continue;
                };
                let project_dir =
                    join_repo_relpath(Path::new(&row.checkout_dir), &scope.bbox_root_relpath);
                if let Err(err) = watcher.unwatch_repo_store(&project_dir) {
                    tracing::warn!(
                        checkout = %project_dir.display(),
                        error = %err,
                        "checkout registry removed but watcher teardown failed"
                    );
                }
            }
        }
        let mut affected_scopes = rows
            .iter()
            .filter_map(CheckoutRow::published_scope)
            .collect::<BTreeSet<_>>();
        for snapshot in self
            .state
            .knowledge_overlays
            .write()
            .remove_checkout(checkout_id)
        {
            affected_scopes.insert(snapshot.key.published_scope);
        }
        for snapshot in self.state.gap_overlays.write().remove_checkout(checkout_id) {
            affected_scopes.insert(snapshot.key.published_scope);
        }
        for scope in affected_scopes {
            // A successful closeout may have advanced the publisher ref. Drop
            // the committed-tree cache before rebuilding so promotion is
            // observed immediately rather than after the cache TTL.
            self.invalidate_published_knowledge_cache(&scope);
            self.reconcile_knowledge_scope_index(&scope);
        }
        Ok(rows.len())
    }

    /// Force the next published view to observe a target ref advanced by
    /// closeout even when push or worktree removal has not completed yet.
    pub(crate) fn refresh_published_knowledge_for_checkout(&self, checkout_id: &str) -> usize {
        let rows = self
            .state
            .checkout_registry
            .read()
            .rows_for_checkout(checkout_id)
            .cloned()
            .collect::<Vec<_>>();
        let scopes = rows
            .iter()
            .filter_map(CheckoutRow::published_scope)
            .collect::<BTreeSet<_>>();
        for scope in &scopes {
            self.invalidate_published_knowledge_cache(scope);
            self.reconcile_knowledge_scope_index(scope);
        }
        for row in rows {
            if let Some(checkout) = self.resolve_registered_checkout(&row) {
                self.refresh_dark_gap_overlay(&checkout);
            }
        }
        scopes.len()
    }

    pub(crate) fn run_knowledge_schema_epoch_inventory(
        &self,
    ) -> Result<bbox_knowledge::inventory::PersistedInventoryReport> {
        let entries = self.state.kb.read().all_entries().to_vec();
        let roots = self
            .state
            .projects
            .read()
            .list()
            .into_iter()
            .map(|project| PathBuf::from(project.canonical_path))
            .collect::<Vec<_>>();
        bbox_knowledge::inventory::persist_schema_epoch_inventory(
            &entries,
            &roots,
            &self.state.store_dir,
            crate::config::read_repo_id_inputs,
        )
    }

    /// Retire path-keyed project authority only after every local and
    /// traveling proof is complete. The host-local cut marker is persisted
    /// before the in-memory flag flips, and its existence makes the cut
    /// monotonic across restarts.
    pub(crate) fn reconcile_path_fallback_cut(
        &self,
        inventory: &bbox_knowledge::inventory::PersistedInventoryReport,
    ) -> Result<PathFallbackCutReport> {
        let already_cut = self.path_fallback_is_cut();
        let mut blockers = Vec::new();
        if !inventory.inventory.quarantined.is_empty() {
            blockers.push(format!(
                "{} quarantined knowledge entries remain",
                inventory.inventory.quarantined.len()
            ));
        }
        let legacy_knowledge = self.state.kb.read().legacy_path_scoped_entry_count()?;
        if legacy_knowledge > 0 {
            blockers.push(format!(
                "{legacy_knowledge} central path-scoped knowledge entries remain"
            ));
        }
        let legacy_gaps = self.state.gaps.read().legacy_path_scoped_entry_count()?;
        if legacy_gaps > 0 {
            blockers.push(format!(
                "{legacy_gaps} central path-scoped gap entries remain"
            ));
        }
        let projects = self.state.projects.read().list();
        if projects.is_empty() {
            blockers.push(
                "no registered project scopes exist; refusing a vacuous path-fallback cut".into(),
            );
        }
        let mut scopes = BTreeSet::new();
        for project in &projects {
            match project_published_scope(project, crate::config::read_repo_id_inputs) {
                Some(scope) => {
                    scopes.insert(scope);
                }
                None => blockers.push(format!(
                    "registered project {} has no recorded published scope",
                    project.canonical_path
                )),
            }
        }
        for scope in scopes {
            let publisher = match self.authorize_publisher(&projects, &scope) {
                Ok(publisher) => publisher,
                Err(err) => {
                    blockers.push(format!(
                        "scope {scope:?} publisher authority failed: {err:#}"
                    ));
                    continue;
                }
            };
            let marker_path = schema_epoch_repo_path(&scope);
            let Some(raw) = bbox_corpus_core::git::read_committed_file(
                Path::new(&publisher.root),
                &publisher.commit,
                &marker_path,
            ) else {
                blockers.push(format!(
                    "scope {scope:?} has no committed schema epoch marker at {}",
                    publisher.branch_ref
                ));
                continue;
            };
            let marker = serde_json::from_str::<bbox_knowledge::inventory::SchemaEpochMarker>(&raw);
            match marker {
                Ok(marker)
                    if marker.schema_epoch == bbox_knowledge::inventory::SCHEMA_EPOCH
                        && marker.repo_id == scope.repo_id
                        && marker.bbox_root_relpath == scope.bbox_root_relpath => {}
                Ok(_) => blockers.push(format!(
                    "scope {scope:?} has a mismatched committed schema epoch marker"
                )),
                Err(err) => blockers.push(format!(
                    "scope {scope:?} has an invalid committed schema epoch marker: {err}"
                )),
            }
        }

        if already_cut {
            return Ok(PathFallbackCutReport {
                cut: true,
                blockers,
                ..Default::default()
            });
        }

        if !blockers.is_empty() {
            return Ok(PathFallbackCutReport {
                blockers,
                ..Default::default()
            });
        }

        // Close the readiness/write race. Store mutations perform their final
        // cut check under these same write locks. A mutation already in flight
        // finishes before this recheck and becomes a blocker; a later mutation
        // observes the store-layer cut and refuses path authority.
        let mut knowledge = self.state.kb.write();
        let mut gaps = self.state.gaps.write();
        let final_knowledge = knowledge.legacy_path_scoped_entry_count()?;
        let final_gaps = gaps.legacy_path_scoped_entry_count()?;
        if final_knowledge > 0 || final_gaps > 0 {
            if final_knowledge > 0 {
                blockers.push(format!(
                    "{final_knowledge} central path-scoped knowledge entries appeared during cut readiness"
                ));
            }
            if final_gaps > 0 {
                blockers.push(format!(
                    "{final_gaps} central path-scoped gap entries appeared during cut readiness"
                ));
            }
            return Ok(PathFallbackCutReport {
                blockers,
                ..Default::default()
            });
        }
        bbox_knowledge::inventory::persist_path_fallback_cut(&self.state.store_dir)?;
        knowledge.set_path_fallback_cut(true);
        gaps.set_path_fallback_cut(true);
        self.state
            .path_fallback_cut
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(PathFallbackCutReport {
            cut: true,
            newly_cut: true,
            blockers,
        })
    }

    pub(crate) fn watch_dark_knowledge_checkout(&self, checkout_project_dir: &Path) {
        let mut watcher = self.state.bbox_watcher.lock().unwrap();
        let Some(watcher) = watcher.as_mut() else {
            return;
        };
        if let Err(err) = watcher.watch_repo_store(checkout_project_dir) {
            tracing::warn!(
                checkout = %checkout_project_dir.display(),
                error = %err,
                "provisional knowledge watcher registration failed"
            );
        }
    }

    pub(crate) fn watch_resolved_dark_knowledge_checkout(&self, checkout: &ResolvedCheckoutScope) {
        self.watch_dark_knowledge_checkout(Path::new(&checkout.checkout_project_dir));
    }

    pub(crate) fn resolved_dark_knowledge_carrier(
        &self,
        checkout: &ResolvedCheckoutScope,
    ) -> String {
        checkout.checkout_project_dir.clone()
    }

    fn resolve_registered_checkout(&self, row: &CheckoutRow) -> Option<ResolvedCheckoutScope> {
        let scope = row.published_scope()?;
        let checkout_dir = canonical_or_original(Path::new(&row.checkout_dir));
        if !checkout_dir.is_dir() {
            return None;
        }
        let marker = checkout_dir.join(".bbox/local/checkout-id");
        if read_checkout_id(&marker).ok().flatten().as_deref() != Some(row.checkout_id.as_str()) {
            return None;
        }
        let checkout_project_dir = join_repo_relpath(&checkout_dir, &scope.bbox_root_relpath);
        let raw = checkout_project_dir.to_str()?;
        let resolution = self.resolve_project_write(raw).ok()?;
        let checkout = resolution.checkout_scope?;
        (checkout.checkout_id == row.checkout_id
            && checkout.published_scope == scope
            && canonical_or_original(Path::new(&checkout.checkout_dir)) == checkout_dir
            && canonical_or_original(Path::new(&checkout.checkout_project_dir))
                == canonical_or_original(&checkout_project_dir))
        .then_some(checkout)
    }

    fn reconcile_knowledge_scope_index(&self, scope: &PublishedScope) {
        let projects = self.state.projects.read().list();
        match self.authorize_publisher(&projects, scope) {
            Ok(publisher) => {
                if let Err(err) = self.sync_knowledge_scope_to_index(scope, &publisher.root) {
                    tracing::warn!(
                        error = %err,
                        scope = ?scope,
                        "checkout lifecycle index convergence failed closed"
                    );
                    self.clear_knowledge_scope_in_index(scope);
                }
            }
            Err(_) => {
                self.clear_knowledge_scope_in_index(scope);
            }
        }
    }
}

fn registry_key(row: &CheckoutRow) -> (String, PublishedScope) {
    (
        row.checkout_id.clone(),
        row.published_scope().unwrap_or(PublishedScope {
            repo_id: String::new(),
            bbox_root_relpath: String::new(),
        }),
    )
}

fn schema_epoch_repo_path(scope: &PublishedScope) -> String {
    if scope.bbox_root_relpath == "." {
        format!(
            ".bbox/knowledge/{}",
            bbox_knowledge::inventory::SCHEMA_EPOCH_MARKER
        )
    } else {
        format!(
            "{}/.bbox/knowledge/{}",
            scope.bbox_root_relpath,
            bbox_knowledge::inventory::SCHEMA_EPOCH_MARKER
        )
    }
}

fn recorded_scope(project: &ProjectRecord) -> Option<PublishedScope> {
    let project_root = Path::new(&project.canonical_path);
    let repo_id = resolve_recorded_repo_id(&crate::config::read_repo_id_inputs(project_root))?;
    let git_root = bbox_corpus_core::git::git_root_for_path(project_root)?;
    let bbox_root_relpath = bbox_root_relpath(&git_root, project_root)?;
    Some(PublishedScope {
        repo_id,
        bbox_root_relpath,
    })
}

fn join_repo_relpath(checkout_dir: &Path, relpath: &str) -> PathBuf {
    if relpath == "." {
        checkout_dir.to_path_buf()
    } else {
        relpath
            .split('/')
            .fold(checkout_dir.to_path_buf(), |path, part| path.join(part))
    }
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
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

    fn fixture() -> (
        tempfile::TempDir,
        BlackboxServer,
        PathBuf,
        PathBuf,
        PublishedScope,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let temp_root = temp.path().canonicalize().unwrap();
        let base = temp_root.join("repo");
        std::fs::create_dir_all(base.join(".bbox/knowledge")).unwrap();
        git(&base, &["init", "-q"]);
        git(&base, &["config", "user.email", "test@example.com"]);
        git(&base, &["config", "user.name", "Test"]);
        std::fs::write(
            base.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-family-lifecycle\"\n",
        )
        .unwrap();
        std::fs::write(base.join(".bbox/knowledge/.gitkeep"), "").unwrap();
        git(&base, &["add", ".bbox"]);
        git(&base, &["commit", "-q", "-m", "seed"]);
        let worktree = temp_root.join("linked");
        git(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "bro-fleet/lifecycle",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );

        let state_dir = temp_root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(SharedState::for_test(&state_dir)));
        server.state.projects.write().register_path(&base).unwrap();
        (
            temp,
            server,
            base,
            worktree,
            PublishedScope {
                repo_id: "repo-family-lifecycle".into(),
                bbox_root_relpath: ".".into(),
            },
        )
    }

    #[test]
    fn reconciliation_recovers_discoverable_rows_and_rejects_reused_identity() {
        let (_temp, server, base, worktree, scope) = fixture();
        assert!(server.state.checkout_registry.read().rows().is_empty());

        let first = server.reconcile_dark_knowledge_checkouts().unwrap();
        assert_eq!(first.discovered, 2, "base and linked checkout discovered");
        assert_eq!(first.dropped, 0);
        assert_eq!(first.refreshed, 2);
        let base_id = bbox_corpus_core::identity::ensure_checkout_id(&base).unwrap();
        let old_worktree_id = bbox_corpus_core::identity::ensure_checkout_id(&worktree).unwrap();
        assert!(
            server
                .state
                .checkout_registry
                .read()
                .get(&base_id, &scope)
                .is_some()
        );
        assert!(
            server
                .state
                .checkout_registry
                .read()
                .get(&old_worktree_id, &scope)
                .is_some()
        );

        let replacement_id = "replacement-checkout-identity";
        std::fs::write(worktree.join(".bbox/local/checkout-id"), replacement_id).unwrap();
        let second = server.reconcile_dark_knowledge_checkouts().unwrap();
        assert_eq!(second.dropped, 1, "marker mismatch drops the stale row");
        assert_eq!(
            second.discovered, 1,
            "live checkout re-registers under its new id"
        );
        let registry = server.state.checkout_registry.read();
        assert!(registry.get(&old_worktree_id, &scope).is_none());
        assert!(registry.get(replacement_id, &scope).is_some());
        drop(registry);
        assert!(
            server
                .state
                .knowledge_overlays
                .read()
                .get(&scope, &old_worktree_id)
                .is_none(),
            "a replacement checkout cannot inherit the old overlay"
        );
        assert!(
            server
                .state
                .knowledge_overlays
                .read()
                .get(&scope, replacement_id)
                .is_some()
        );
    }

    #[test]
    fn explicit_teardown_removes_every_scope_and_overlay() {
        let (_temp, server, _base, worktree, scope) = fixture();
        server.reconcile_dark_knowledge_checkouts().unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&worktree).unwrap();
        assert_eq!(
            server.refresh_published_knowledge_for_checkout(&checkout_id),
            1,
            "closeout refresh resolves the checkout's published scope"
        );
        assert_eq!(
            server
                .deregister_dark_knowledge_checkout(&checkout_id)
                .unwrap(),
            1
        );
        assert!(
            server
                .state
                .checkout_registry
                .read()
                .get(&checkout_id, &scope)
                .is_none()
        );
        assert!(
            server
                .state
                .knowledge_overlays
                .read()
                .get(&scope, &checkout_id)
                .is_none()
        );
    }

    #[test]
    fn live_scope_ignores_uncommitted_config_edits() {
        let (_temp, server, base, _worktree, scope) = fixture();
        std::fs::write(
            base.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"working-tree-only\"\n",
        )
        .unwrap();

        let project = server.state.projects.read().list().pop().unwrap();
        assert_eq!(recorded_scope(&project), Some(scope.clone()));
        assert!(server.authorize_publisher(&[project], &scope).is_ok());
    }

    #[test]
    fn pinned_config_scope_mismatch_fails_closed() {
        let (_temp, server, base, _worktree, scope) = fixture();
        let project = server.state.projects.read().list().pop().unwrap();
        let pin = server.authorize_publisher(std::slice::from_ref(&project), &scope);
        assert!(pin.is_ok(), "initial publisher authority: {pin:?}");

        let pinned_branch = bbox_corpus_core::git::current_branch(&base).unwrap();
        git(&base, &["switch", "-q", "-c", "candidate-head"]);
        git(&base, &["switch", "-q", &pinned_branch]);
        std::fs::write(
            base.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"different-committed-scope\"\n",
        )
        .unwrap();
        git(&base, &["add", ".bbox/config.toml"]);
        git(&base, &["commit", "-q", "-m", "move pinned scope"]);
        git(&base, &["switch", "-q", "candidate-head"]);

        let err = server.authorize_publisher(&[project], &scope).unwrap_err();
        assert!(
            err.to_string()
                .contains("committed project scope does not match")
        );
    }

    #[test]
    fn startup_recovery_does_not_wait_for_a_live_transaction_lane() {
        use fs2::FileExt;

        let (_temp, server, _base, worktree, _scope) = fixture();
        let transaction_root = worktree.join(".bbox/local/knowledge-transactions");
        std::fs::create_dir_all(&transaction_root).unwrap();
        let pending = transaction_root.join("pending.json");
        std::fs::write(&pending, b"{\"version\":").unwrap();
        let lane = std::fs::File::open(&transaction_root).unwrap();
        lane.lock_exclusive().unwrap();

        let started = std::time::Instant::now();
        assert_eq!(server.recover_abandoned_dark_knowledge_transactions(), 0);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "startup recovery waited for a live transaction lane"
        );
        assert!(
            pending.exists(),
            "the live owner's pointer must remain intact"
        );

        drop(lane);
        assert_eq!(server.recover_abandoned_dark_knowledge_transactions(), 0);
        assert!(
            !pending.exists(),
            "a later periodic pass must clear the abandoned pointer"
        );
    }

    #[test]
    fn path_fallback_cut_waits_for_committed_marker_and_is_monotonic() {
        let (_temp, server, base, _worktree, _scope) = fixture();
        let inventory = server.run_knowledge_schema_epoch_inventory().unwrap();
        assert!(base.join(".bbox/knowledge/.schema-epoch").is_file());

        let blocked = server.reconcile_path_fallback_cut(&inventory).unwrap();
        assert!(!blocked.cut);
        assert!(
            blocked
                .blockers
                .iter()
                .any(|blocker| blocker.contains("no committed schema epoch marker")),
            "{:?}",
            blocked.blockers
        );

        git(&base, &["add", ".bbox/knowledge/.schema-epoch"]);
        git(&base, &["commit", "-q", "-m", "record schema epoch"]);
        let cut = server.reconcile_path_fallback_cut(&inventory).unwrap();
        assert!(cut.cut);
        assert!(cut.newly_cut);
        assert!(server.path_fallback_is_cut());
        assert!(bbox_knowledge::inventory::path_fallback_was_cut(&server.state.store_dir).unwrap());

        let repeated = server.reconcile_path_fallback_cut(&inventory).unwrap();
        assert!(repeated.cut);
        assert!(!repeated.newly_cut);
    }

    #[test]
    fn path_fallback_cut_refuses_empty_project_registry() {
        let temp = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(SharedState::for_test(temp.path())));
        let inventory = server.run_knowledge_schema_epoch_inventory().unwrap();

        let report = server.reconcile_path_fallback_cut(&inventory).unwrap();

        assert!(!report.cut);
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.contains("vacuous path-fallback cut")),
            "{:?}",
            report.blockers
        );
    }
}
