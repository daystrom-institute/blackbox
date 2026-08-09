//! Catalog-mode [`ProjectRecordsProvider`]: the runtime compatibility
//! projection (phase-2 §6.2).
//!
//! Builds `ProjectRecord` rows from the strict catalog/attachment pair
//! exclusively through `validate_catalog_attachments` plus
//! `ProjectRecord::from_catalog_attachment`, the exact P1-D §7.2 contract:
//! one row per project with exactly one active `Base` attachment, no row
//! and an omitted count for remote-only projects, and the complete catalog
//! id set beside the rows so corpus identity surfaces are never gated by
//! attachment presence.
//!
//! Divergence from the migration-time projection checker: a project with
//! multiple active base attachments degrades to an omitted row here (with
//! a warning) instead of failing the whole projection. The migration
//! checker's hard refusal guards import parity; at runtime one ambiguous
//! project must not unpublish every other record.
//!
//! Snapshots are derived on read against the catalog epoch, so staleness
//! is unrepresentable and admin commits publish without a republish call.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bbox_corpus_core::code_project_identity::CodeProjectIdentity;
use bbox_corpus_core::project_catalog::{
    AttachmentKind, AttachmentStatus, validate_catalog_attachments,
};
use bbox_corpus_core::project_record::{
    ProjectRecord, ProjectRecordsProvider, ProjectRecordsSnapshot,
};

use crate::project_catalog_store::ProjectCatalogStore;

pub struct CatalogProjectRecordsProvider {
    store: Arc<ProjectCatalogStore>,
    git_transport_cutover: Arc<crate::git_transport_cutover::GitTransportCutoverRuntimeV1>,
    cache: parking_lot::Mutex<Option<ProjectRecordsSnapshot>>,
    /// Most recent degradation (stale-cache serving, cross-validation
    /// failure, omitted rows), surfaced through doctor/health.
    degradation: parking_lot::Mutex<Option<String>>,
}

impl CatalogProjectRecordsProvider {
    pub fn new(store: Arc<ProjectCatalogStore>) -> Self {
        Self::new_with_git_transport_cutover(
            store,
            Arc::new(crate::git_transport_cutover::GitTransportCutoverRuntimeV1::default()),
        )
    }

    pub fn new_with_git_transport_cutover(
        store: Arc<ProjectCatalogStore>,
        git_transport_cutover: Arc<crate::git_transport_cutover::GitTransportCutoverRuntimeV1>,
    ) -> Self {
        Self {
            store,
            git_transport_cutover,
            cache: parking_lot::Mutex::new(None),
            degradation: parking_lot::Mutex::new(None),
        }
    }
}

impl ProjectRecordsProvider for CatalogProjectRecordsProvider {
    fn records_snapshot(&self) -> ProjectRecordsSnapshot {
        let mut cache = self.cache.lock();
        let state = match self.store.snapshot() {
            Ok(state) => state,
            Err(error) => {
                // A poisoned store serves the last good snapshot when one
                // exists; a boot-time failure yields the empty snapshot and
                // the strict open has already refused startup.
                tracing::warn!(code = %error.code(), "catalog snapshot unavailable");
                *self.degradation.lock() = Some(format!(
                    "catalog snapshot unavailable ({}); serving the last good projection",
                    error.code()
                ));
                return cache.clone().unwrap_or_else(ProjectRecordsSnapshot::empty);
            }
        };
        if let Some(snapshot) = cache.as_ref()
            && snapshot.authority_epoch == state.epoch()
        {
            return snapshot.clone();
        }

        let catalog = state.catalog();
        let attachments = state.attachments();
        let mut records = Vec::new();
        let mut omitted = 0_u64;
        match validate_catalog_attachments(catalog, attachments) {
            Ok(validated) => {
                for project in catalog.projects.values() {
                    let base_ids = attachments
                        .attachments
                        .values()
                        .filter(|attachment| {
                            attachment.project_id == project.project_id
                                && attachment.status == AttachmentStatus::Attached
                                && attachment.kind == AttachmentKind::Base
                        })
                        .map(|attachment| attachment.attachment_id.clone())
                        .collect::<Vec<_>>();
                    let [base_id] = base_ids.as_slice() else {
                        if base_ids.len() > 1 {
                            tracing::warn!(
                                project_id = %project.project_id,
                                bases = base_ids.len(),
                                "project has multiple active base attachments; omitted from \
                                 the compatibility projection"
                            );
                        }
                        omitted = omitted.saturating_add(1);
                        continue;
                    };
                    let Some(attachment) = validated.attachment(base_id) else {
                        omitted = omitted.saturating_add(1);
                        continue;
                    };
                    match ProjectRecord::from_catalog_attachment(project, attachment) {
                        Ok(record) => records.push(record),
                        Err(error) => {
                            tracing::warn!(
                                project_id = %project.project_id,
                                %error,
                                "compatibility join refused; project omitted"
                            );
                            omitted = omitted.saturating_add(1);
                        }
                    }
                }
            }
            Err(error) => {
                // Unreachable after a strict open plus transact-only
                // mutation; fail closed to an empty projection rather than
                // serving unvalidated rows.
                tracing::error!(code = %error.code(), "catalog pair failed cross-validation");
                omitted = catalog.projects.len() as u64;
            }
        }
        records.sort_by(|left, right| left.canonical_path.cmp(&right.canonical_path));
        let corpus_project_ids = catalog
            .projects
            .keys()
            .map(|id| id.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let snapshot = ProjectRecordsSnapshot {
            records: Arc::new(records),
            corpus_project_ids: Arc::new(corpus_project_ids),
            omitted_catalog_count: omitted,
            authority_epoch: state.epoch(),
        };
        *cache = Some(snapshot.clone());
        *self.degradation.lock() = (omitted > 0)
            .then(|| format!("compatibility projection omitted {omitted} catalog project(s)"));
        snapshot
    }

    fn git_history_transport_governed(&self, project_id: &str) -> bool {
        if self.git_transport_cutover.marker().is_none() {
            return false;
        }
        let Ok(state) = self.store.snapshot() else {
            // A selected marker proves that some catalog history is governed.
            // If the catalog cannot map this attached compatibility row back
            // to its repository, suppressing the speculative lease is the
            // only fail-closed answer.
            return true;
        };
        let Ok(project_id) = bbox_corpus_core::project_catalog::ProjectId::parse(project_id) else {
            return true;
        };
        let Some(project) = state.catalog().projects.get(&project_id) else {
            return true;
        };
        matches!(
            project.scope,
            bbox_corpus_core::project_catalog::ProjectScope::Published(_)
        ) && project
            .repo_history
            .as_ref()
            .is_some_and(|repo_history_id| self.git_transport_cutover.covers_repo(repo_history_id))
    }

    /// Catalog derivation of the planning identity map (Phase 3 plan
    /// section 7 item 1): one entry per catalog project, INCLUDING projects
    /// with zero attachments, which the compatibility `records` projection
    /// deliberately omits. Read live off the catalog store rather than the
    /// cached record snapshot, because the cache holds the attached-only
    /// projection and would silently drop exactly the remote-only projects
    /// this map exists to serve.
    fn code_identities(&self) -> BTreeMap<String, CodeProjectIdentity> {
        let state = match self.store.snapshot() {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    code = %error.code(),
                    "catalog snapshot unavailable; planning identity map is empty this pass"
                );
                return BTreeMap::new();
            }
        };
        let catalog = state.catalog();
        catalog
            .projects
            .values()
            .map(|project| {
                let repo_history = project
                    .repo_history
                    .as_ref()
                    .and_then(|id| catalog.repo_histories.get(id));
                (
                    project.project_id.as_str().to_string(),
                    CodeProjectIdentity::from_catalog(project, repo_history),
                )
            })
            .collect()
    }

    fn last_degradation(&self) -> Option<String> {
        self.degradation.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, CatalogSnapshotV2, CheckoutAttachment, CorpusProject,
        ProjectId, ProjectScope,
    };
    use std::path::Path;

    const ATTACHED: &str = "p_000000000000000000000000000000a1";
    const REMOTE: &str = "p_000000000000000000000000000000b1";
    const ATTACHMENT: &str = "att_00000000000000000000000000000a01";

    fn project(id: &str) -> CorpusProject {
        CorpusProject {
            project_id: ProjectId::parse(id).unwrap(),
            scope: ProjectScope::LegacyLocal,
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: format!("p {id}"),
            created_at: "2026-07-24T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: Default::default(),
        }
    }

    fn base_attachment(root: &Path) -> CheckoutAttachment {
        CheckoutAttachment {
            attachment_id: AttachmentId::parse(ATTACHMENT).unwrap(),
            project_id: ProjectId::parse(ATTACHED).unwrap(),
            checkout_id: "feed00000000000000000000000000a1".into(),
            checkout_dir: root.to_str().unwrap().into(),
            checkout_project_dir: root.to_str().unwrap().into(),
            project_root_relpath: ".".into(),
            kind: AttachmentKind::Base,
            validated_scope: None,
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: AttachmentCapabilities::default(),
            status: AttachmentStatus::Attached,
            attached_at: "2026-07-24T00:00:00Z".into(),
            detached_at: None,
        }
    }

    fn fixture_store(root: &Path) -> Arc<ProjectCatalogStore> {
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog: &mut CatalogSnapshotV2, attachments| {
                catalog
                    .projects
                    .insert(ProjectId::parse(ATTACHED).unwrap(), project(ATTACHED));
                catalog
                    .projects
                    .insert(ProjectId::parse(REMOTE).unwrap(), project(REMOTE));
                attachments.attachments.insert(
                    AttachmentId::parse(ATTACHMENT).unwrap(),
                    base_attachment(&checkout),
                );
                Ok(())
            })
            .unwrap();
        Arc::new(store)
    }

    #[test]
    fn attached_rows_omitted_remote_and_full_id_set() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = fixture_store(&root);
        let provider = CatalogProjectRecordsProvider::new(store);

        let snapshot = provider.records_snapshot();
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].project_id, ATTACHED);
        assert_eq!(
            snapshot.records[0].canonical_path,
            root.join("checkout").to_str().unwrap()
        );
        assert_eq!(snapshot.omitted_catalog_count, 1);
        assert!(snapshot.corpus_project_ids.contains(ATTACHED));
        assert!(snapshot.corpus_project_ids.contains(REMOTE));
    }

    #[test]
    fn epoch_change_rebuilds_without_republish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = fixture_store(&root);
        let provider = CatalogProjectRecordsProvider::new(store.clone());

        let first = provider.records_snapshot();
        assert_eq!(first.records.len(), 1);

        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_, attachments| {
                let row = attachments
                    .attachments
                    .get_mut(&AttachmentId::parse(ATTACHMENT).unwrap())
                    .unwrap();
                row.status = AttachmentStatus::Detached;
                row.detached_at = Some("2026-07-24T00:00:01Z".into());
                row.capabilities = AttachmentCapabilities::default();
                Ok(())
            })
            .unwrap();

        let second = provider.records_snapshot();
        assert!(second.records.is_empty());
        assert_eq!(second.omitted_catalog_count, 2);
        assert!(second.authority_epoch > first.authority_epoch);
        // The full id set is attachment-independent.
        assert_eq!(second.corpus_project_ids.len(), 2);
    }
}
