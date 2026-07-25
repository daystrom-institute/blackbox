//! Catalog administration vocabulary
//! (design/daemon-runtime/durable-project-catalog-phase2-impl.md §7).
//!
//! Every operation is: validate inputs, build complete post-images inside a
//! [`ProjectCatalogStore::transact`] closure, return a typed receipt. No
//! operation writes either snapshot directly, holds a lock across
//! filesystem probing, or performs probing itself: the daemon tool layer
//! probes checkouts off-lock (checkout-id marker, committed scope, kind
//! detection, capability observation) and passes the results in as data,
//! the same injected-closure pattern the schema-epoch inventory set.
//!
//! Epoch discipline (plan §7.1): these dedicated admin operations require
//! the caller-supplied expected epoch and refuse stale epochs with the
//! store's typed compare-and-swap failure; nothing here retries with a
//! fresh epoch.

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, CheckoutAttachment,
    ProjectId, ProjectScope,
};

use crate::project_catalog_store::{
    ProjectCatalogCommit, ProjectCatalogStore, ProjectCatalogStoreError,
};

pub type AdminResult<T> = std::result::Result<T, ProjectCatalogStoreError>;

fn admin_error(code: &'static str, detail: impl Into<String>) -> ProjectCatalogStoreError {
    ProjectCatalogStoreError::new(code, detail.into())
}

/// Daemon-probed facts about the checkout being attached. Probing runs
/// off-lock in the tool layer; the transaction closure revalidates the
/// pure catalog invariants against these values.
#[derive(Debug, Clone)]
pub struct AttachProbe {
    /// Durable checkout identity from `.bbox/local/checkout-id` (minted by
    /// the tool layer when absent, via the shared identity helper).
    pub checkout_id: String,
    /// Canonical checkout top.
    pub checkout_dir: String,
    /// Canonical project dir inside the checkout.
    pub checkout_project_dir: String,
    /// Monorepo discriminator relative to the checkout top (`.` at root).
    pub project_root_relpath: String,
    pub kind: AttachmentKind,
    /// Committed recorded scope resolved at `HEAD`, when the checkout
    /// records authority. Strict cross-validation requires this to equal a
    /// `Published` project's scope exactly and to be absent for
    /// `LegacyLocal`.
    pub validated_scope: Option<PublishedScope>,
    pub computed_repo_hint: Option<bbox_corpus_core::project_catalog::RepoBootstrapHint>,
    pub branch_ref: Option<String>,
    /// Capabilities observed at attach time; acquisition still revalidates
    /// each capability at lease time.
    pub capabilities: AttachmentCapabilities,
    /// Bounded timestamp supplied by the caller (the daemon clock).
    pub attached_at: String,
}

#[derive(Debug, Clone)]
pub struct AttachReceipt {
    pub attachment_id: AttachmentId,
    pub commit: ProjectCatalogCommit,
}

/// Attach one probed checkout to an existing catalog project (plan §7.3).
pub fn attach_checkout(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    probe: &AttachProbe,
) -> AdminResult<AttachReceipt> {
    let attachment_id = AttachmentId::mint();
    let receipt_id = attachment_id.clone();
    let project_id = project_id.clone();
    let probe = probe.clone();
    let commit = store.transact(expected_epoch, move |catalog, attachments| {
        let Some(project) = catalog.projects.get(&project_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                format!("project {project_id} is not in the catalog"),
            ));
        };
        match (&project.scope, &probe.validated_scope) {
            (ProjectScope::Published(scope), Some(probed)) if scope == probed => {}
            (ProjectScope::Published(_), Some(_)) => {
                return Err(admin_error(
                    "error.project_catalog_admin_scope_mismatch",
                    "checkout resolves a different published scope; use the scope \
                     migration or promotion surface instead of attach",
                ));
            }
            (ProjectScope::Published(_), None) => {
                return Err(admin_error(
                    "error.project_catalog_admin_scope_required",
                    "a checkout without committed recorded authority cannot attach \
                     to a published project",
                ));
            }
            (ProjectScope::LegacyLocal, None) => {}
            (ProjectScope::LegacyLocal, Some(_)) => {
                return Err(admin_error(
                    "error.project_catalog_admin_promotion_required",
                    "checkout records committed authority; promote the legacy-local \
                     project instead of attaching new authority silently",
                ));
            }
        }
        for existing in attachments.attachments.values() {
            if existing.status != AttachmentStatus::Attached {
                continue;
            }
            if existing.project_id == project_id
                && existing.checkout_id == probe.checkout_id
                && existing.project_root_relpath == probe.project_root_relpath
            {
                return Err(admin_error(
                    "error.project_catalog_admin_attachment_exists",
                    format!(
                        "attachment {} already covers this checkout",
                        existing.attachment_id
                    ),
                ));
            }
            if existing.project_id != project_id
                && existing.checkout_id == probe.checkout_id
                && existing.project_root_relpath == probe.project_root_relpath
            {
                return Err(admin_error(
                    "error.project_catalog_admin_checkout_claimed",
                    "another project already claims this checkout and relpath",
                ));
            }
        }
        attachments.attachments.insert(
            receipt_id.clone(),
            CheckoutAttachment {
                attachment_id: receipt_id.clone(),
                project_id: project_id.clone(),
                checkout_id: probe.checkout_id.clone(),
                checkout_dir: probe.checkout_dir.clone(),
                checkout_project_dir: probe.checkout_project_dir.clone(),
                project_root_relpath: probe.project_root_relpath.clone(),
                kind: probe.kind.clone(),
                validated_scope: probe.validated_scope.clone(),
                computed_repo_hint: probe.computed_repo_hint.clone(),
                branch_ref: probe.branch_ref.clone(),
                capabilities: probe.capabilities.clone(),
                status: AttachmentStatus::Attached,
                attached_at: probe.attached_at.clone(),
                detached_at: None,
            },
        );
        Ok(())
    })?;
    Ok(AttachReceipt {
        attachment_id,
        commit,
    })
}

/// Detach one attachment (plan §7.3): flips status, stamps `detached_at`,
/// clears capability bits (strict validation forbids a detached row that
/// claims active capability state), clears any default-attachment selection
/// pointing at it, and leaves every logical store, entity ref, and
/// generation untouched.
pub fn detach_attachment(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    attachment_id: &AttachmentId,
    detached_at: &str,
) -> AdminResult<ProjectCatalogCommit> {
    let attachment_id = attachment_id.clone();
    let detached_at = detached_at.to_string();
    store.transact(expected_epoch, move |_catalog, attachments| {
        let Some(row) = attachments.attachments.get_mut(&attachment_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_attachment",
                format!("attachment {attachment_id} is not in the store"),
            ));
        };
        if row.status == AttachmentStatus::Detached {
            return Err(admin_error(
                "error.project_catalog_admin_already_detached",
                format!("attachment {attachment_id} is already detached"),
            ));
        }
        row.status = AttachmentStatus::Detached;
        row.detached_at = Some(detached_at.clone());
        row.capabilities = AttachmentCapabilities::default();
        attachments
            .default_attachments
            .retain(|_, selected| selected != &attachment_id);
        Ok(())
    })
}

/// Record or clear the operator-selected default local-source attachment
/// for one project (plan §7.3).
pub fn set_default_attachment(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    selection: Option<&AttachmentId>,
) -> AdminResult<ProjectCatalogCommit> {
    let project_id = project_id.clone();
    let selection = selection.cloned();
    store.transact(expected_epoch, move |catalog, attachments| {
        if !catalog.projects.contains_key(&project_id) {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                format!("project {project_id} is not in the catalog"),
            ));
        }
        match &selection {
            None => {
                attachments.default_attachments.remove(&project_id);
            }
            Some(attachment_id) => {
                let Some(row) = attachments.attachments.get(attachment_id) else {
                    return Err(admin_error(
                        "error.project_catalog_admin_unknown_attachment",
                        format!("attachment {attachment_id} is not in the store"),
                    ));
                };
                if row.project_id != project_id {
                    return Err(admin_error(
                        "error.project_catalog_admin_attachment_project_mismatch",
                        "default selection must name an attachment of the same project",
                    ));
                }
                if row.status != AttachmentStatus::Attached {
                    return Err(admin_error(
                        "error.project_catalog_admin_attachment_detached",
                        "a detached attachment cannot be the default",
                    ));
                }
                attachments
                    .default_attachments
                    .insert(project_id.clone(), attachment_id.clone());
            }
        }
        Ok(())
    })
}

/// Everything promotion needs beyond the catalog itself, probed off-lock
/// by the tool layer: the committed authority each active attachment of
/// the project currently resolves (None when unreadable or unrecorded),
/// plus the bridge generation ids when the project has an active collected
/// generation or accepted publication pointer.
#[derive(Debug, Clone)]
pub struct PromotionEvidence {
    pub attachment_scopes: std::collections::BTreeMap<AttachmentId, Option<PublishedScope>>,
    pub code_bridge_generation: Option<String>,
    pub publication_bridge_generation: Option<String>,
    pub operator_invocation: String,
    pub operator_reason: Option<String>,
    pub proved_at: String,
}

#[derive(Debug, Clone)]
pub struct ScopeTransitionReceipt {
    pub scope_migration_id: bbox_corpus_core::project_catalog::ScopeMigrationId,
    pub commit: ProjectCatalogCommit,
}

/// Promote one attached `LegacyLocal` project to its newly committed scope
/// (plan §7.4, governing §7.2, D-012). One pair transaction flips the same
/// record to `Published`, writes the typed promotion record plus its
/// attachment proof, and performs the repo-history authority transition of
/// governing §5.1. Sibling attachments must all prove the exact proposed
/// scope; sibling projects sharing the history record must not carry
/// conflicting published authority.
pub fn promote_project(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    designated_attachment: &AttachmentId,
    proposed_scope: &PublishedScope,
    evidence: &PromotionEvidence,
) -> AdminResult<ScopeTransitionReceipt> {
    use bbox_corpus_core::project_catalog::{
        CommitNamespace, RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId,
        RepoHistoryRecord, ScopeMigrationAttachmentProof, ScopeMigrationAuthorityProvenance,
        ScopeMigrationId, ScopeMigrationKind, ScopeMigrationRecord,
    };

    let migration_id = ScopeMigrationId::mint();
    let receipt_id = migration_id.clone();
    let project_id = project_id.clone();
    let designated = designated_attachment.clone();
    let proposed = proposed_scope.clone();
    let evidence = evidence.clone();
    let new_epoch = expected_epoch.checked_add(1).ok_or_else(|| {
        admin_error(
            "error.project_catalog_admin_epoch_overflow",
            "catalog epoch cannot be incremented",
        )
    })?;
    let commit = store.transact(expected_epoch, move |catalog, attachments| {
        let Some(project) = catalog.projects.get(&project_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                format!("project {project_id} is not in the catalog"),
            ));
        };
        if project.scope != ProjectScope::LegacyLocal {
            return Err(admin_error(
                "error.project_catalog_admin_not_legacy_local",
                "promotion applies only to a legacy-local project",
            ));
        }
        // The proposed scope must be unowned: refusal, never a merge.
        if catalog.projects.values().any(
            |other| matches!(&other.scope, ProjectScope::Published(s) if s == &proposed),
        ) {
            return Err(admin_error(
                "error.project_catalog_admin_scope_owned",
                "another project already owns this scope; use the offline \
                 compatibility resolution workflow",
            ));
        }
        // Every active attachment must prove the exact proposed scope and
        // carry its relpath; the designated attachment cannot overrule
        // siblings (governing §7.2).
        let active: Vec<&CheckoutAttachment> = attachments
            .attachments
            .values()
            .filter(|row| row.status == AttachmentStatus::Attached && row.project_id == project_id)
            .collect();
        let Some(designated_row) = active
            .iter()
            .find(|row| row.attachment_id == designated)
        else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_attachment",
                "the designated promotion attachment is not an active attachment \
                 of this project",
            ));
        };
        let _ = designated_row;
        for row in &active {
            if row.project_root_relpath != proposed.bbox_root_relpath() {
                return Err(admin_error(
                    "error.project_catalog_admin_promotion_ambiguous",
                    format!(
                        "attachment {} carries relpath {} but the proposed scope \
                         names {}",
                        row.attachment_id,
                        row.project_root_relpath,
                        proposed.bbox_root_relpath()
                    ),
                ));
            }
            match evidence.attachment_scopes.get(&row.attachment_id) {
                Some(Some(scope)) if scope == &proposed => {}
                _ => {
                    return Err(admin_error(
                        "error.project_catalog_admin_promotion_ambiguous",
                        format!(
                            "attachment {} does not prove the proposed scope; detach \
                             or repair it first",
                            row.attachment_id
                        ),
                    ));
                }
            }
        }
        // Repo-history transition (governing §5.1): find-or-create the
        // recorded record without changing any established identity.
        let authority = RecordedRepoAuthority::parse(proposed.repo_id())
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        let project_history = project.repo_history.clone();
        match &project_history {
            Some(history_id) => {
                for other in catalog.projects.values() {
                    if other.project_id == project_id
                        || other.repo_history.as_ref() != Some(history_id)
                    {
                        continue;
                    }
                    let conflicting = match &other.scope {
                        ProjectScope::LegacyLocal => false,
                        ProjectScope::Published(scope) => {
                            scope.repo_id() != proposed.repo_id()
                        }
                    };
                    if conflicting {
                        return Err(admin_error(
                            "error.project_catalog_admin_history_conflict",
                            "a sibling project referencing the shared history record \
                             carries conflicting repository authority",
                        ));
                    }
                }
                let Some(record) = catalog.repo_histories.get_mut(history_id) else {
                    return Err(admin_error(
                        "error.project_catalog_admin_history_missing",
                        "the project references a history record absent from the catalog",
                    ));
                };
                // Authority becomes recorded; the stable id, primary
                // namespace, and compatibility namespaces never change.
                record.authority = RepoHistoryAuthority::Recorded(authority);
            }
            None => {
                let existing = catalog
                    .repo_histories
                    .iter()
                    .find(|(_, record)| {
                        matches!(&record.authority, RepoHistoryAuthority::Recorded(a) if a.as_str() == proposed.repo_id())
                    })
                    .map(|(id, _)| id.clone());
                let history_id = match existing {
                    Some(id) => id,
                    None => {
                        let id = RepoHistoryId::mint();
                        let primary = CommitNamespace::parse(proposed.repo_id())
                            .map_err(|error| admin_error(error.code(), error.to_string()))?;
                        catalog.repo_histories.insert(
                            id.clone(),
                            RepoHistoryRecord {
                                repo_history_id: id.clone(),
                                authority: RepoHistoryAuthority::Recorded(authority),
                                primary_namespace: primary,
                                compatibility_namespaces: Default::default(),
                            },
                        );
                        id
                    }
                };
                let project = catalog.projects.get_mut(&project_id).expect("checked above");
                project.repo_history = Some(history_id);
            }
        }
        // Flip the same record, stamp attachment scopes, and write the
        // typed audit chain in this one transaction.
        let project = catalog.projects.get_mut(&project_id).expect("checked above");
        project.scope = ProjectScope::Published(proposed.clone());
        let mut checkout_id = String::new();
        for row in attachments.attachments.values_mut() {
            if row.status == AttachmentStatus::Attached && row.project_id == project_id {
                row.validated_scope = Some(proposed.clone());
                if row.attachment_id == designated {
                    checkout_id = row.checkout_id.clone();
                }
            }
        }
        catalog.scope_migrations.insert(
            receipt_id.clone(),
            ScopeMigrationRecord {
                scope_migration_id: receipt_id.clone(),
                project_id: project_id.clone(),
                catalog_epoch: new_epoch,
                authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
                operator_invocation: evidence.operator_invocation.clone(),
                operator_reason: evidence.operator_reason.clone(),
                old_scope: ProjectScope::LegacyLocal,
                new_scope: ProjectScope::Published(proposed.clone()),
                kind: ScopeMigrationKind::Promotion,
                migrated_at: evidence.proved_at.clone(),
                code_bridge_generation: evidence.code_bridge_generation.clone(),
                publication_bridge_generation: evidence.publication_bridge_generation.clone(),
                pending_capabilities: Default::default(),
            },
        );
        attachments.scope_migration_proofs.insert(
            receipt_id.clone(),
            ScopeMigrationAttachmentProof {
                scope_migration_id: receipt_id.clone(),
                attachment_id: designated.clone(),
                checkout_id,
                old_scope: ProjectScope::LegacyLocal,
                new_scope: ProjectScope::Published(proposed.clone()),
                proved_at: evidence.proved_at.clone(),
            },
        );
        Ok(())
    })?;
    Ok(ScopeTransitionReceipt {
        scope_migration_id: migration_id,
        commit,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, CorpusProject};
    use std::path::Path;
    use std::sync::Arc;

    const PROJECT: &str = "p_000000000000000000000000000000a1";
    const OTHER: &str = "p_000000000000000000000000000000b1";
    const CHECKOUT: &str = "feed00000000000000000000000000a1";

    fn project(id: &str, scope: ProjectScope) -> CorpusProject {
        CorpusProject {
            project_id: ProjectId::parse(id).unwrap(),
            scope,
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: format!("p {id}"),
            created_at: "2026-07-24T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: Default::default(),
        }
    }

    fn probe(root: &Path) -> AttachProbe {
        AttachProbe {
            checkout_id: CHECKOUT.into(),
            checkout_dir: root.to_str().unwrap().into(),
            checkout_project_dir: root.to_str().unwrap().into(),
            project_root_relpath: ".".into(),
            kind: AttachmentKind::Base,
            validated_scope: None,
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: AttachmentCapabilities {
                local_code_source: true,
                ..AttachmentCapabilities::default()
            },
            attached_at: "2026-07-24T00:00:00Z".into(),
        }
    }

    fn store_with_projects(root: &Path) -> Arc<ProjectCatalogStore> {
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
                catalog.projects.insert(
                    ProjectId::parse(PROJECT).unwrap(),
                    project(PROJECT, ProjectScope::LegacyLocal),
                );
                catalog.projects.insert(
                    ProjectId::parse(OTHER).unwrap(),
                    project(OTHER, ProjectScope::LegacyLocal),
                );
                Ok(())
            })
            .unwrap();
        Arc::new(store)
    }

    fn current_epoch(store: &ProjectCatalogStore) -> u64 {
        store.snapshot().unwrap().epoch()
    }

    #[test]
    fn attach_detach_and_default_selection_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("checkout")).unwrap();
        let store = store_with_projects(&root);
        let project_id = ProjectId::parse(PROJECT).unwrap();

        let receipt = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("checkout")),
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let row = state
            .attachments()
            .attachments
            .get(&receipt.attachment_id)
            .unwrap();
        assert_eq!(row.status, AttachmentStatus::Attached);
        assert_eq!(row.checkout_id, CHECKOUT);

        // Duplicate active coverage refuses.
        let error = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("checkout")),
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_attachment_exists"
        );

        // Cross-project claim of the same (checkout, relpath) refuses.
        let error = attach_checkout(
            &store,
            current_epoch(&store),
            &ProjectId::parse(OTHER).unwrap(),
            &probe(&root.join("checkout")),
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_checkout_claimed");

        // Default selection round trip.
        set_default_attachment(
            &store,
            current_epoch(&store),
            &project_id,
            Some(&receipt.attachment_id),
        )
        .unwrap();
        assert_eq!(
            store
                .snapshot()
                .unwrap()
                .attachments()
                .default_attachments
                .get(&project_id),
            Some(&receipt.attachment_id)
        );

        // Detach flips status, clears capabilities, and clears the default.
        detach_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            "2026-07-24T00:00:01Z",
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let row = state
            .attachments()
            .attachments
            .get(&receipt.attachment_id)
            .unwrap();
        assert_eq!(row.status, AttachmentStatus::Detached);
        assert!(!row.capabilities.any());
        assert!(state.attachments().default_attachments.is_empty());

        // Detaching again refuses; stale epochs refuse.
        let error = detach_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            "2026-07-24T00:00:02Z",
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_already_detached");
        let error = detach_attachment(&store, 1, &receipt.attachment_id, "t").unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_stale_epoch");
    }

    #[test]
    fn promotion_flips_scope_writes_audit_chain_and_creates_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("checkout")).unwrap();
        let store = store_with_projects(&root);
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let receipt = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("checkout")),
        )
        .unwrap();

        let scope = PublishedScope::try_new("promotedfamily", ".").unwrap();
        let evidence = PromotionEvidence {
            attachment_scopes: [(receipt.attachment_id.clone(), Some(scope.clone()))]
                .into_iter()
                .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:promote".into(),
            operator_reason: None,
            proved_at: "2026-07-24T00:00:03Z".into(),
        };
        let transition = promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &receipt.attachment_id,
            &scope,
            &evidence,
        )
        .unwrap();

        let state = store.snapshot().unwrap();
        let project = state.catalog().projects.get(&project_id).unwrap();
        assert_eq!(project.scope, ProjectScope::Published(scope.clone()));
        let history_id = project.repo_history.as_ref().expect("history created");
        let history = state.catalog().repo_histories.get(history_id).unwrap();
        assert_eq!(history.primary_namespace.as_str(), "promotedfamily");
        let record = state
            .catalog()
            .scope_migrations
            .get(&transition.scope_migration_id)
            .unwrap();
        assert_eq!(record.catalog_epoch, state.epoch());
        assert!(
            state
                .attachments()
                .scope_migration_proofs
                .contains_key(&transition.scope_migration_id)
        );
        let row = state
            .attachments()
            .attachments
            .get(&receipt.attachment_id)
            .unwrap();
        assert_eq!(row.validated_scope.as_ref(), Some(&scope));

        // Promoting again refuses: no longer legacy-local.
        let error = promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &receipt.attachment_id,
            &scope,
            &evidence,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_not_legacy_local");
    }

    #[test]
    fn promotion_refuses_owned_scope_and_disagreeing_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let store = store_with_projects(&root);
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let other_id = ProjectId::parse(OTHER).unwrap();

        let first = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("a")),
        )
        .unwrap();
        let mut second_probe = probe(&root.join("b"));
        second_probe.checkout_id = "feed00000000000000000000000000b2".into();
        let second =
            attach_checkout(&store, current_epoch(&store), &project_id, &second_probe).unwrap();

        let scope = PublishedScope::try_new("familyone", ".").unwrap();
        // Sibling attachment does not prove the scope: ambiguous.
        let partial = PromotionEvidence {
            attachment_scopes: [(first.attachment_id.clone(), Some(scope.clone()))]
                .into_iter()
                .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:promote".into(),
            operator_reason: None,
            proved_at: "2026-07-24T00:00:03Z".into(),
        };
        let error = promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &first.attachment_id,
            &scope,
            &partial,
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_promotion_ambiguous"
        );

        // Owned scope refuses rather than forking or merging.
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                let other = catalog.projects.get_mut(&other_id).unwrap();
                other.scope = ProjectScope::Published(scope.clone());
                Ok(())
            })
            .unwrap();
        let full = PromotionEvidence {
            attachment_scopes: [
                (first.attachment_id.clone(), Some(scope.clone())),
                (second.attachment_id.clone(), Some(scope.clone())),
            ]
            .into_iter()
            .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:promote".into(),
            operator_reason: None,
            proved_at: "2026-07-24T00:00:04Z".into(),
        };
        let error = promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &first.attachment_id,
            &scope,
            &full,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_owned");
    }

    #[test]
    fn published_scope_gates_on_attach() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("checkout")).unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();
        let scope = PublishedScope::try_new("repofamily77", ".").unwrap();
        let published = ProjectId::parse(PROJECT).unwrap();
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                catalog.projects.insert(
                    published.clone(),
                    project(PROJECT, ProjectScope::Published(scope.clone())),
                );
                catalog.projects.insert(
                    ProjectId::parse(OTHER).unwrap(),
                    project(OTHER, ProjectScope::LegacyLocal),
                );
                Ok(())
            })
            .unwrap();

        // No committed authority: cannot attach to a published project.
        let error = attach_checkout(
            &store,
            current_epoch(&store),
            &published,
            &probe(&root.join("checkout")),
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_required");

        // Wrong committed scope refuses with the migration pointer.
        let mut wrong = probe(&root.join("checkout"));
        wrong.validated_scope = Some(PublishedScope::try_new("otherfamily", ".").unwrap());
        let error = attach_checkout(&store, current_epoch(&store), &published, &wrong).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_mismatch");

        // Exact committed scope attaches.
        let mut right = probe(&root.join("checkout"));
        right.validated_scope = Some(scope);
        attach_checkout(&store, current_epoch(&store), &published, &right).unwrap();

        // A committed-authority checkout cannot silently attach to a
        // legacy-local project: promotion is the explicit surface.
        let mut promoted = probe(&root.join("checkout2"));
        promoted.checkout_id = "feed00000000000000000000000000b2".into();
        promoted.validated_scope = Some(PublishedScope::try_new("thirdfamily", ".").unwrap());
        let error = attach_checkout(
            &store,
            current_epoch(&store),
            &ProjectId::parse(OTHER).unwrap(),
            &promoted,
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_promotion_required"
        );
    }
}
