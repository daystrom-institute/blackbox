use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bbox_corpus_core::identity::{
    PublishedScope, bbox_root_relpath, read_checkout_id, resolve_recorded_repo_id,
};
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::checkout_registry::{CheckoutRow, discover_checkout_dirs};
use bbox_indexing::publisher::project_published_scope;
use bbox_knowledge::knowledge::{KnowledgeEntry, Scope};

use super::{BlackboxServer, KnowledgeOverlayRefreshOutcome};

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

const PUBLISHER_AUTHORIZATION_CACHE_TTL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub(crate) struct AuthorizedPublisher {
    pub(crate) root: String,
    pub(crate) branch_ref: String,
    pub(crate) commit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublisherAuthorizationErrorKind {
    InvalidAuthority,
    Transient,
}

#[derive(Debug)]
pub(crate) struct PublisherAuthorizationError {
    pub(crate) kind: PublisherAuthorizationErrorKind,
    diagnostic: String,
}

impl PublisherAuthorizationError {
    fn invalid(diagnostic: impl Into<String>) -> Self {
        Self {
            kind: PublisherAuthorizationErrorKind::InvalidAuthority,
            diagnostic: diagnostic.into(),
        }
    }

    fn transient(error: anyhow::Error) -> Self {
        Self {
            kind: PublisherAuthorizationErrorKind::Transient,
            diagnostic: format!("{error:#}"),
        }
    }

    pub(crate) fn is_transient(&self) -> bool {
        self.kind == PublisherAuthorizationErrorKind::Transient
    }
}

impl std::fmt::Display for PublisherAuthorizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for PublisherAuthorizationError {}

#[derive(Clone)]
pub(crate) struct PublisherAuthorizationCacheEntry {
    project_inventory: Vec<(String, String)>,
    checked_at: Instant,
    publisher: AuthorizedPublisher,
}

#[derive(Default)]
pub(crate) struct PublisherAuthorizationCache {
    entries: BTreeMap<PublishedScope, PublisherAuthorizationCacheEntry>,
    generations: BTreeMap<PublishedScope, u64>,
}

impl PublisherAuthorizationCache {
    fn generation(&self, scope: &PublishedScope) -> u64 {
        self.generations.get(scope).copied().unwrap_or_default()
    }

    fn cached(
        &self,
        scope: &PublishedScope,
        project_inventory: &[(String, String)],
    ) -> Option<AuthorizedPublisher> {
        let now = Instant::now();
        self.entries
            .get(scope)
            .filter(|entry| {
                entry.project_inventory == project_inventory
                    && now.duration_since(entry.checked_at) < PUBLISHER_AUTHORIZATION_CACHE_TTL
            })
            .map(|entry| entry.publisher.clone())
    }

    fn insert_if_generation(
        &mut self,
        scope: PublishedScope,
        expected_generation: u64,
        entry: PublisherAuthorizationCacheEntry,
    ) -> bool {
        if self.generation(&scope) != expected_generation {
            return false;
        }
        self.entries.insert(scope, entry);
        true
    }

    pub(crate) fn invalidate(&mut self, scope: &PublishedScope) {
        let generation = self.generation(scope).checked_add(1).unwrap_or(1);
        self.generations.insert(scope.clone(), generation);
        self.entries.remove(scope);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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
        self.authorize_publisher_classified(projects, scope)
            .map_err(anyhow::Error::new)
    }

    pub(crate) fn authorize_publisher_classified(
        &self,
        projects: &[ProjectRecord],
        scope: &PublishedScope,
    ) -> std::result::Result<AuthorizedPublisher, PublisherAuthorizationError> {
        let project_inventory = publisher_project_inventory(projects);
        loop {
            let generation = {
                let cache = self.state.publisher_authorization_cache.read();
                if let Some(publisher) = cache.cached(scope, &project_inventory) {
                    return Ok(publisher);
                }
                cache.generation(scope)
            };

            let publisher = self.resolve_authorized_publisher(projects, scope)?;
            let mut cache = self.state.publisher_authorization_cache.write();
            if !cache.insert_if_generation(
                scope.clone(),
                generation,
                PublisherAuthorizationCacheEntry {
                    project_inventory: project_inventory.clone(),
                    checked_at: Instant::now(),
                    publisher: publisher.clone(),
                },
            ) {
                continue;
            }
            return Ok(publisher);
        }
    }

    fn resolve_authorized_publisher(
        &self,
        projects: &[ProjectRecord],
        scope: &PublishedScope,
    ) -> std::result::Result<AuthorizedPublisher, PublisherAuthorizationError> {
        let mut matches = Vec::new();
        for project in projects {
            if committed_project_scope_at_ref(project, "HEAD")?.as_ref() == Some(scope) {
                matches.push(project.canonical_path.clone());
            }
        }
        matches.sort();
        let root = match matches.as_slice() {
            [] => {
                return Err(PublisherAuthorizationError::invalid(format!(
                    "no publisher for scope {scope:?}"
                )));
            }
            [root] => root.clone(),
            _ => {
                return Err(PublisherAuthorizationError::invalid(format!(
                    "duplicate publishers for scope {scope:?}: {}",
                    matches.join(", ")
                )));
            }
        };
        let pin = self
            .state
            .publisher_refs
            .write()
            .ensure_pinned(scope, Path::new(&root))
            .with_context(|| format!("pinning publisher for scope {scope:?}"))
            .map_err(PublisherAuthorizationError::transient)?;
        let commit = resolve_pinned_commit(Path::new(&root), &pin.branch_ref)?;
        let project = projects
            .iter()
            .find(|project| project.canonical_path == root)
            .ok_or_else(|| {
                PublisherAuthorizationError::invalid(format!(
                    "publisher {root} vanished during authority resolution"
                ))
            })?;
        let pinned_scope = committed_project_scope_at_ref(project, &commit)?.ok_or_else(|| {
            PublisherAuthorizationError::invalid(format!(
                "publisher ref {} has no recorded repo authority at {}",
                pin.branch_ref, commit
            ))
        })?;
        if &pinned_scope != scope {
            return Err(PublisherAuthorizationError::invalid(format!(
                "publisher ref {} resolves to commit {} whose committed project scope does not match {scope:?}",
                pin.branch_ref, commit
            )));
        }
        let publisher = AuthorizedPublisher {
            root,
            branch_ref: pin.branch_ref,
            commit,
        };
        Ok(publisher)
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
        }
        let mut refreshed_scopes = BTreeSet::new();
        let mut fallback_scopes = BTreeSet::new();
        for row in rows {
            if let Some(checkout) = self.resolve_registered_checkout(&row) {
                refreshed_scopes.insert(checkout.published_scope.clone());
                let outcome = self.refresh_dark_knowledge_overlay(&checkout);
                if matches!(
                    outcome,
                    KnowledgeOverlayRefreshOutcome::PreservedTransient
                        | KnowledgeOverlayRefreshOutcome::Superseded
                ) {
                    fallback_scopes.insert(checkout.published_scope.clone());
                }
                self.refresh_dark_gap_overlay(&checkout);
            }
        }
        for scope in scopes.difference(&refreshed_scopes) {
            fallback_scopes.insert(scope.clone());
        }
        for scope in fallback_scopes {
            self.reconcile_knowledge_scope_index(&scope);
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
            // The migration cut is monotonic and security-sensitive. It must
            // inspect the current committed marker, never a short-lived hot
            // read authority decision from before the marker commit.
            self.invalidate_publisher_authority_cache(&scope);
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
        match self.authorize_publisher_classified(&projects, scope) {
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
            Err(err) if err.is_transient() => {
                tracing::warn!(
                    error = %err,
                    scope = ?scope,
                    "checkout lifecycle index convergence degraded; preserving prior scope index"
                );
            }
            Err(_) => {
                self.clear_knowledge_scope_in_index(scope);
            }
        }
    }
}

fn publisher_project_inventory(projects: &[ProjectRecord]) -> Vec<(String, String)> {
    let mut inventory = projects
        .iter()
        .map(|project| (project.project_id.clone(), project.canonical_path.clone()))
        .collect::<Vec<_>>();
    inventory.sort();
    inventory
}

fn committed_project_scope_at_ref(
    project: &ProjectRecord,
    reference: &str,
) -> std::result::Result<Option<PublishedScope>, PublisherAuthorizationError> {
    let project_root = Path::new(&project.canonical_path);
    let output = bbox_corpus_core::git::git_output(
        project_root,
        &["rev-parse", "--show-toplevel"],
        "resolving publisher repository root",
    )
    .ok_or_else(|| {
        PublisherAuthorizationError::transient(anyhow::anyhow!(
            "publisher repository root could not be checked at {}",
            project_root.display()
        ))
    })?;
    if !output.status.success() {
        return Err(PublisherAuthorizationError::invalid(format!(
            "registered publisher candidate {} is not inside a Git repository: {}",
            project_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let git_root = String::from_utf8(output.stdout).map_err(|err| {
        PublisherAuthorizationError::invalid(format!(
            "publisher repository root is not UTF-8 for {}: {err}",
            project_root.display()
        ))
    })?;
    let git_root = std::fs::canonicalize(git_root.trim()).map_err(|err| {
        PublisherAuthorizationError::transient(anyhow::anyhow!(
            "canonicalizing publisher repository root for {}: {err}",
            project_root.display()
        ))
    })?;
    let bbox_root_relpath = bbox_root_relpath(&git_root, project_root).ok_or_else(|| {
        PublisherAuthorizationError::invalid(format!(
            "publisher candidate {} is outside its Git root {}",
            project_root.display(),
            git_root.display()
        ))
    })?;
    let config_relpath = if bbox_root_relpath == "." {
        ".bbox/config.toml".to_string()
    } else {
        format!("{bbox_root_relpath}/.bbox/config.toml")
    };
    let spec = format!("{reference}:{config_relpath}");
    let output = bbox_corpus_core::git::git_output(
        &git_root,
        &["show", &spec],
        "reading committed publisher authority",
    )
    .ok_or_else(|| {
        PublisherAuthorizationError::transient(anyhow::anyhow!(
            "committed publisher authority could not be read from {}",
            project_root.display()
        ))
    })?;
    if !output.status.success() {
        return Ok(None);
    }
    let source = String::from_utf8(output.stdout).map_err(|err| {
        PublisherAuthorizationError::invalid(format!(
            "committed publisher config {config_relpath} is not UTF-8: {err}"
        ))
    })?;
    let inputs = crate::config::repo_id_inputs_from_project_config_source(project_root, &source)
        .map_err(|err| {
            PublisherAuthorizationError::invalid(format!(
                "parsing committed publisher config {config_relpath}: {err:#}"
            ))
        })?;
    let Some(repo_id) = resolve_recorded_repo_id(&inputs) else {
        return Ok(None);
    };
    Ok(Some(PublishedScope {
        repo_id,
        bbox_root_relpath,
    }))
}

fn resolve_pinned_commit(
    root: &Path,
    branch_ref: &str,
) -> std::result::Result<String, PublisherAuthorizationError> {
    let spec = format!("{branch_ref}^{{commit}}");
    let output = bbox_corpus_core::git::git_output(
        root,
        &["rev-parse", "--verify", &spec],
        "resolving pinned publisher ref",
    )
    .ok_or_else(|| {
        PublisherAuthorizationError::transient(anyhow::anyhow!(
            "publisher ref {branch_ref} could not be checked in {}",
            root.display()
        ))
    })?;
    if !output.status.success() {
        return Err(PublisherAuthorizationError::invalid(format!(
            "publisher ref {branch_ref} does not resolve in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let commit = String::from_utf8(output.stdout).map_err(|err| {
        PublisherAuthorizationError::invalid(format!(
            "publisher ref {branch_ref} returned a non-UTF-8 commit id: {err}"
        ))
    })?;
    let commit = commit.trim();
    if commit.is_empty() {
        return Err(PublisherAuthorizationError::invalid(format!(
            "publisher ref {branch_ref} resolved to an empty commit id"
        )));
    }
    Ok(commit.to_string())
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
    use bbox_knowledge::knowledge::{Approval, Category, Priority, Status};
    use bbox_knowledge::overlay::{OverlayStatus, OverlayValue};
    use std::collections::HashMap;

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

    fn write_test_knowledge(root: &Path, id: &str, content: &str) {
        let entry = KnowledgeEntry {
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
            render: false,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-07-21T00:00:00Z".into(),
            updated_at: "2026-07-21T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_vec_pretty(&entry).unwrap(),
        )
        .unwrap();
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
    fn closeout_refresh_recomputes_knowledge_overlay_after_publisher_advances() {
        let (_temp, server, base, worktree, scope) = fixture();
        server.reconcile_dark_knowledge_checkouts().unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&worktree).unwrap();

        write_test_knowledge(&worktree, "closeout-refresh", "closeout convergence");
        let row = server
            .state
            .checkout_registry
            .read()
            .get(&checkout_id, &scope)
            .cloned()
            .unwrap();
        let checkout = server.resolve_registered_checkout(&row).unwrap();
        server.refresh_dark_knowledge_overlay(&checkout);
        assert!(matches!(
            server
                .state
                .knowledge_overlays
                .read()
                .get(&scope, &checkout_id)
                .and_then(|snapshot| snapshot.values.get("closeout-refresh")),
            Some(OverlayValue::Upsert { .. })
        ));

        git(&worktree, &["add", ".bbox/knowledge/closeout-refresh.json"]);
        git(&worktree, &["commit", "-q", "-m", "publish knowledge"]);
        git(&base, &["merge", "-q", "--ff-only", "bro-fleet/lifecycle"]);

        assert_eq!(
            server.refresh_published_knowledge_for_checkout(&checkout_id),
            1
        );
        let refreshed = server
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &checkout_id)
            .cloned()
            .unwrap();
        assert_eq!(refreshed.status, OverlayStatus::Valid);
        assert!(
            refreshed.values.is_empty(),
            "promoted knowledge must leave the provisional overlay immediately"
        );
    }

    #[test]
    fn closeout_transient_overlay_refresh_reconciles_current_publisher_index() {
        let (_temp, server, base, worktree, scope) = fixture();
        server.reconcile_dark_knowledge_checkouts().unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&worktree).unwrap();
        let row = server
            .state
            .checkout_registry
            .read()
            .get(&checkout_id, &scope)
            .cloned()
            .unwrap();
        let checkout = server.resolve_registered_checkout(&row).unwrap();

        write_test_knowledge(&worktree, "closeout-fallback", "CURRENT PUBLISHER FALLBACK");
        server.refresh_dark_knowledge_overlay(&checkout);
        git(
            &worktree,
            &["add", ".bbox/knowledge/closeout-fallback.json"],
        );
        git(&worktree, &["commit", "-q", "-m", "publish fallback"]);
        git(&base, &["merge", "-q", "--ff-only", "bro-fleet/lifecycle"]);

        let transaction_root = worktree.join(".bbox/local/knowledge-transactions");
        std::fs::create_dir_all(&transaction_root).unwrap();
        std::fs::write(transaction_root.join("pending.json"), b"{}").unwrap();
        assert_eq!(
            server.refresh_published_knowledge_for_checkout(&checkout_id),
            1
        );
        server.state.index_writer.flush_blocking().unwrap();

        let preserved = server
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &checkout_id)
            .cloned()
            .unwrap();
        assert_eq!(preserved.status, OverlayStatus::Valid);
        assert!(
            preserved
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("refresh degraded"))
        );
        let hits = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("CURRENT PUBLISHER FALLBACK", 10, Some("knowledge"))
            .unwrap();
        assert!(
            hits.iter().any(|hit| {
                hit.entity_id == crate::index::knowledge_entity_id("closeout-fallback")
            }),
            "fallback reconcile must index the current publisher commit: {hits:?}"
        );
    }

    #[test]
    fn transient_refresh_preserves_valid_overlay_but_malformed_content_replaces_it() {
        let (_temp, server, base, worktree, scope) = fixture();
        server.reconcile_dark_knowledge_checkouts().unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&worktree).unwrap();
        let row = server
            .state
            .checkout_registry
            .read()
            .get(&checkout_id, &scope)
            .cloned()
            .unwrap();
        let checkout = server.resolve_registered_checkout(&row).unwrap();
        let prior = server
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &checkout_id)
            .cloned()
            .unwrap();
        assert_eq!(prior.status, OverlayStatus::Valid);

        let unavailable = worktree.with_extension("unavailable");
        std::fs::rename(&worktree, &unavailable).unwrap();
        assert_eq!(
            server.refresh_dark_knowledge_overlay(&checkout),
            KnowledgeOverlayRefreshOutcome::PreservedTransient
        );
        let preserved = server
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &checkout_id)
            .cloned()
            .unwrap();
        assert_eq!(preserved.status, OverlayStatus::Valid);
        assert_eq!(preserved.snapshot_id, prior.snapshot_id);
        assert_eq!(preserved.values.len(), prior.values.len());
        assert!(
            preserved
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("refresh degraded"))
        );
        std::fs::rename(&unavailable, &worktree).unwrap();
        let degraded_view = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("all"))
            .unwrap();
        assert!(
            degraded_view
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("refresh degraded"))
        );

        std::fs::write(worktree.join(".bbox/knowledge/broken.json"), b"{").unwrap();
        assert_eq!(
            server.refresh_dark_knowledge_overlay(&checkout),
            KnowledgeOverlayRefreshOutcome::Invalid
        );
        let invalid = server
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &checkout_id)
            .cloned()
            .unwrap();
        assert_eq!(invalid.status, OverlayStatus::Invalid);
        assert!(invalid.values.is_empty());
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("broken.json"))
        );
    }

    #[test]
    fn invalid_publisher_authority_replaces_prior_valid_overlay() {
        let (_temp, server, base, worktree, scope) = fixture();
        server.reconcile_dark_knowledge_checkouts().unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&worktree).unwrap();
        let row = server
            .state
            .checkout_registry
            .read()
            .get(&checkout_id, &scope)
            .cloned()
            .unwrap();
        let checkout = server.resolve_registered_checkout(&row).unwrap();
        assert_eq!(
            server
                .state
                .knowledge_overlays
                .read()
                .get(&scope, &checkout_id)
                .unwrap()
                .status,
            OverlayStatus::Valid
        );

        write_test_knowledge(&base, "authority-visible", "AUTHORITY VISIBLE CONTENT");
        git(&base, &["add", ".bbox/knowledge/authority-visible.json"]);
        git(&base, &["commit", "-q", "-m", "publish authority fixture"]);
        server.invalidate_published_knowledge_cache(&scope);
        server
            .sync_knowledge_scope_to_index(&scope, base.to_str().unwrap())
            .unwrap();
        server.state.index_writer.flush_blocking().unwrap();
        let before = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("AUTHORITY VISIBLE CONTENT", 10, Some("knowledge"))
            .unwrap();
        assert!(before.iter().any(|hit| {
            hit.entity_id == crate::index::knowledge_entity_id("authority-visible")
        }));

        std::fs::write(
            base.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"different-publisher-scope\"\n",
        )
        .unwrap();
        git(&base, &["add", ".bbox/config.toml"]);
        git(&base, &["commit", "-q", "-m", "change publisher scope"]);
        server.invalidate_published_knowledge_cache(&scope);

        assert_eq!(
            server.refresh_dark_knowledge_overlay(&checkout),
            KnowledgeOverlayRefreshOutcome::Invalid
        );
        let invalid = server
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &checkout_id)
            .cloned()
            .unwrap();
        assert_eq!(invalid.status, OverlayStatus::Invalid);
        assert!(invalid.values.is_empty());
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("no publisher"))
        );
        server.state.index_writer.flush_blocking().unwrap();
        let after = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("AUTHORITY VISIBLE CONTENT", 10, Some("knowledge"))
            .unwrap();
        assert!(
            after.iter().all(|hit| {
                hit.entity_id != crate::index::knowledge_entity_id("authority-visible")
            }),
            "invalid publisher authority must clear static scope documents: {after:?}"
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

        server.invalidate_published_knowledge_cache(&scope);
        let err = server.authorize_publisher(&[project], &scope).unwrap_err();
        assert!(
            err.to_string()
                .contains("committed project scope does not match")
        );
    }

    #[test]
    fn missing_pinned_branch_is_invalid_authority_not_transient() {
        let (_temp, server, base, _worktree, scope) = fixture();
        let project = server.state.projects.read().list().pop().unwrap();
        let publisher = server
            .authorize_publisher(std::slice::from_ref(&project), &scope)
            .unwrap();
        let pinned_branch = publisher
            .branch_ref
            .strip_prefix("refs/heads/")
            .unwrap()
            .to_string();
        git(&base, &["switch", "-q", "-c", "replacement-publisher"]);
        git(&base, &["branch", "-D", &pinned_branch]);
        server.invalidate_published_knowledge_cache(&scope);

        let error = server
            .authorize_publisher_classified(&[project], &scope)
            .unwrap_err();
        assert!(!error.is_transient(), "{error:#}");
        assert!(error.to_string().contains("does not resolve"), "{error:#}");
    }

    #[test]
    fn authority_cache_rejects_insert_from_pre_invalidation_generation() {
        let scope = PublishedScope {
            repo_id: "repo-generation".into(),
            bbox_root_relpath: ".".into(),
        };
        let mut cache = PublisherAuthorizationCache::default();
        let stale_generation = cache.generation(&scope);
        cache.invalidate(&scope);
        let inserted = cache.insert_if_generation(
            scope,
            stale_generation,
            PublisherAuthorizationCacheEntry {
                project_inventory: vec![("project".into(), "/repo".into())],
                checked_at: Instant::now(),
                publisher: AuthorizedPublisher {
                    root: "/repo".into(),
                    branch_ref: "refs/heads/main".into(),
                    commit: "a".repeat(40),
                },
            },
        );
        assert!(!inserted);
        assert!(cache.is_empty());
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
