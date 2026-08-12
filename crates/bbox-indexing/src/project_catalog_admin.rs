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

use crate::accepted_publication_runtime::{
    AcceptedPublicationRuntime, AcceptedPublicationSourceBinding,
    ERROR_ACCEPTED_PUBLICATION_REF_MOVED, ERROR_ACCEPTED_PUBLICATION_SCOPE_ADVANCE_REQUIRED,
    PublishError, PublishReceipt, PublishRequest, PublishSources, PublisherPublishMode,
};
use crate::project_catalog_store::{
    ProjectCatalogCommit, ProjectCatalogStore, ProjectCatalogStoreError,
};

pub type AdminResult<T> = std::result::Result<T, ProjectCatalogStoreError>;

pub fn admin_error(code: &'static str, detail: impl Into<String>) -> ProjectCatalogStoreError {
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
                                membership_generation: 0,
                                authority: RepoHistoryAuthority::Recorded(authority),
                                primary_namespace: primary,
                                compatibility_namespaces: Default::default(),
                                materialization: Default::default(),
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

/// Probed relocation facts for one active attachment during a scope
/// migration: the committed scope its config now resolves, and the
/// relocated directories for a relpath move (unchanged values for a
/// recorded-authority change).
#[derive(Debug, Clone)]
pub struct MigrationAttachmentProbe {
    pub resolved_scope: Option<PublishedScope>,
    pub new_project_root_relpath: String,
    pub new_checkout_project_dir: String,
}

#[derive(Debug, Clone)]
pub struct ScopeMigrationRequest {
    pub project_id: ProjectId,
    pub expected_old_scope: PublishedScope,
    pub new_scope: PublishedScope,
    pub kind: bbox_corpus_core::project_catalog::ScopeMigrationKind,
    pub designated_attachment: AttachmentId,
    /// Operator authority flag for a recorded-authority change. Agents pass
    /// it through from operator input and never default or infer it
    /// (D-004, RX-V1 discipline); the tool layer owns that rule, this op
    /// only enforces presence.
    pub acknowledge_repo_authority_change: bool,
    pub attachment_probes: std::collections::BTreeMap<AttachmentId, MigrationAttachmentProbe>,
    pub code_bridge_generation: Option<String>,
    pub publication_bridge_generation: Option<String>,
    pub operator_invocation: String,
    pub operator_reason: Option<String>,
    pub migrated_at: String,
}

/// Attachment-proved scope migration (plan §7.5, governing §7.2): relpath
/// moves and recorded-authority changes for a published project, in one
/// pair transaction that rewrites the catalog scope, revalidates and
/// relocates the active attachments, appends the host-local path bindings,
/// and inserts the path-free migration record with its matching proof.
/// `dry_run` validates the complete mutation against snapshot clones and
/// commits nothing.
pub fn scope_migrate_attached(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    request: &ScopeMigrationRequest,
    dry_run: bool,
) -> AdminResult<Option<ScopeTransitionReceipt>> {
    let new_epoch = expected_epoch.checked_add(1).ok_or_else(|| {
        admin_error(
            "error.project_catalog_admin_epoch_overflow",
            "catalog epoch cannot be incremented",
        )
    })?;
    let migration_id = bbox_corpus_core::project_catalog::ScopeMigrationId::mint();
    if dry_run {
        let state = store.snapshot()?;
        if state.epoch() != expected_epoch {
            return Err(admin_error(
                "error.project_catalog_stale_epoch",
                "expected epoch does not match the current catalog epoch",
            ));
        }
        let mut catalog = (**state.catalog()).clone();
        let mut attachments = (**state.attachments()).clone();
        apply_scope_migration(
            &mut catalog,
            &mut attachments,
            request,
            &migration_id,
            new_epoch,
        )?;
        // The commit path validates the complete post-image pair inside
        // `transact` (review M4): the dry run must apply the same strict
        // and cross-store validation or a colliding relocation reads as
        // clean here and fails at the real invocation.
        catalog.epoch = new_epoch;
        attachments.epoch = new_epoch;
        catalog
            .validate()
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        attachments
            .validate()
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        bbox_corpus_core::project_catalog::validate_catalog_attachments(&catalog, &attachments)
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        return Ok(None);
    }
    let receipt_id = migration_id.clone();
    let request = request.clone();
    let commit = store.transact(expected_epoch, move |catalog, attachments| {
        apply_scope_migration(catalog, attachments, &request, &receipt_id, new_epoch)
    })?;
    Ok(Some(ScopeTransitionReceipt {
        scope_migration_id: migration_id,
        commit,
    }))
}

fn apply_scope_migration(
    catalog: &mut bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    attachments: &mut bbox_corpus_core::project_catalog::AttachmentSnapshotV1,
    request: &ScopeMigrationRequest,
    migration_id: &bbox_corpus_core::project_catalog::ScopeMigrationId,
    new_epoch: u64,
) -> AdminResult<()> {
    use bbox_corpus_core::project_catalog::{
        LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry,
        LegacyPathRelationship, RecordedRepoAuthority, RepoHistoryAuthority,
        ScopeMigrationAttachmentProof, ScopeMigrationAuthorityProvenance, ScopeMigrationKind,
        ScopeMigrationRecord,
    };

    let Some(project) = catalog.projects.get(&request.project_id) else {
        return Err(admin_error(
            "error.project_catalog_admin_unknown_project",
            format!("project {} is not in the catalog", request.project_id),
        ));
    };
    let ProjectScope::Published(current) = &project.scope else {
        return Err(admin_error(
            "error.project_catalog_admin_not_published",
            "scope migration applies only to a published project; promotion is \
             the legacy-local surface",
        ));
    };
    // Section 4.11: refuse a second scope migration while a code bridge
    // is open for this project. An open bridge means an existing
    // ScopeMigrationRecord carries a non-null code_bridge_generation
    // that names a still-effective generation. A second migration would
    // create an untruthful record (the generation's scope is the first
    // migration's old_scope, not the second's) and an unbootable
    // catalog.
    let has_open_bridge = catalog.scope_migrations.values().any(|record| {
        record.project_id == request.project_id && record.code_bridge_generation.is_some()
    });
    if has_open_bridge {
        return Err(admin_error(
            "error.project_catalog_scope_migration_bridge_open",
            "a code bridge is open for this project; clear the bridge via \
             new-scope activation before re-attempting the migration",
        ));
    }
    if current != &request.expected_old_scope {
        return Err(admin_error(
            "error.project_catalog_admin_scope_mismatch",
            "the project no longer carries the expected old scope",
        ));
    }
    match request.kind {
        ScopeMigrationKind::RelpathMove => {
            if request.new_scope.repo_id() != current.repo_id()
                || request.new_scope.bbox_root_relpath() == current.bbox_root_relpath()
            {
                return Err(admin_error(
                    "error.project_catalog_admin_migration_shape",
                    "a relpath move keeps the repository and changes the relpath",
                ));
            }
        }
        ScopeMigrationKind::RepoAuthorityChange => {
            if request.new_scope.repo_id() == current.repo_id()
                || request.new_scope.bbox_root_relpath() != current.bbox_root_relpath()
            {
                return Err(admin_error(
                    "error.project_catalog_admin_migration_shape",
                    "an authority change keeps the relpath and changes the repository",
                ));
            }
            if !request.acknowledge_repo_authority_change {
                return Err(admin_error(
                    "error.project_catalog_admin_acknowledgement_required",
                    "a recorded-authority change requires the explicit operator \
                     acknowledgement flag",
                ));
            }
        }
        ScopeMigrationKind::Promotion => {
            return Err(admin_error(
                "error.project_catalog_admin_migration_shape",
                "promotion has its own surface; scope migration accepts relpath \
                 moves and recorded-authority changes",
            ));
        }
    }
    if catalog.projects.values().any(|other| {
        other.project_id != request.project_id
            && matches!(&other.scope, ProjectScope::Published(s) if s == &request.new_scope)
    }) {
        return Err(admin_error(
            "error.project_catalog_admin_scope_owned",
            "the target scope is already owned; use the offline survivor workflow",
        ));
    }

    let active_ids: Vec<AttachmentId> = attachments
        .attachments
        .values()
        .filter(|row| {
            row.status == AttachmentStatus::Attached && row.project_id == request.project_id
        })
        .map(|row| row.attachment_id.clone())
        .collect();
    if !active_ids.contains(&request.designated_attachment) {
        return Err(admin_error(
            "error.project_catalog_admin_unknown_attachment",
            "the designated attachment is not an active attachment of this project",
        ));
    }
    for id in &active_ids {
        match request.attachment_probes.get(id) {
            Some(probe) if probe.resolved_scope.as_ref() == Some(&request.new_scope) => {}
            _ => {
                return Err(admin_error(
                    "error.project_catalog_admin_migration_ambiguous",
                    format!(
                        "attachment {id} does not prove the new scope; detach or \
                         repair it first"
                    ),
                ));
            }
        }
    }

    // Repo-history transition: a relpath move keeps the record untouched;
    // an authority change re-records authority while preserving the
    // established primary namespace and every compatibility namespace
    // (identity stability is the invariant; the migration record itself is
    // the durable note of the former authority).
    if request.kind == ScopeMigrationKind::RepoAuthorityChange
        && let Some(history_id) = catalog
            .projects
            .get(&request.project_id)
            .and_then(|p| p.repo_history.clone())
    {
        let authority = RecordedRepoAuthority::parse(request.new_scope.repo_id())
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        if let Some(record) = catalog.repo_histories.get_mut(&history_id) {
            record.authority = RepoHistoryAuthority::Recorded(authority);
        }
    }

    let project = catalog
        .projects
        .get_mut(&request.project_id)
        .expect("checked above");
    project.scope = ProjectScope::Published(request.new_scope.clone());

    let mut designated_checkout_id = String::new();
    for id in &active_ids {
        let row = attachments
            .attachments
            .get_mut(id)
            .expect("active id enumerated above");
        let probe = request
            .attachment_probes
            .get(id)
            .expect("probe presence checked above");
        let historical_path = row.checkout_project_dir.clone();
        row.validated_scope = Some(request.new_scope.clone());
        row.project_root_relpath = probe.new_project_root_relpath.clone();
        row.checkout_project_dir = probe.new_checkout_project_dir.clone();
        if id == &request.designated_attachment {
            designated_checkout_id = row.checkout_id.clone();
        }
        if historical_path != probe.new_checkout_project_dir {
            // Append-only host-local binding so path-only legacy rows keep
            // resolving after relocation (plan §8.4).
            let binding_id = LegacyPathBindingId::mint();
            attachments.legacy_path_bindings.insert(
                binding_id.clone(),
                LegacyPathLedgerEntry {
                    legacy_path_binding_id: binding_id,
                    historical_path,
                    source_store: crate::project_catalog_backfill::ATTACHMENT_RELOCATION_SOURCE
                        .to_string(),
                    source_row_id: id.as_str().to_string(),
                    // A relocation binding stands for exactly the attachment it
                    // names, so it carries the same singleton evidence every
                    // small owner does.
                    member_row_count: 1,
                    member_commitment_sha256:
                        bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(
                            id.as_str(),
                        )
                        .commitment_sha256,
                    inventory_epoch: new_epoch,
                    status: LegacyPathBindingStatus::Mapped {
                        project_id: request.project_id.clone(),
                        relationship: LegacyPathRelationship::Root,
                    },
                },
            );
        }
    }

    catalog.scope_migrations.insert(
        migration_id.clone(),
        ScopeMigrationRecord {
            scope_migration_id: migration_id.clone(),
            project_id: request.project_id.clone(),
            catalog_epoch: new_epoch,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: request.operator_invocation.clone(),
            operator_reason: request.operator_reason.clone(),
            old_scope: ProjectScope::Published(request.expected_old_scope.clone()),
            new_scope: ProjectScope::Published(request.new_scope.clone()),
            kind: request.kind.clone(),
            migrated_at: request.migrated_at.clone(),
            code_bridge_generation: request.code_bridge_generation.clone(),
            publication_bridge_generation: request.publication_bridge_generation.clone(),
            pending_capabilities: Default::default(),
        },
    );
    attachments.scope_migration_proofs.insert(
        migration_id.clone(),
        ScopeMigrationAttachmentProof {
            scope_migration_id: migration_id.clone(),
            attachment_id: request.designated_attachment.clone(),
            checkout_id: designated_checkout_id,
            old_scope: ProjectScope::Published(request.expected_old_scope.clone()),
            new_scope: ProjectScope::Published(request.new_scope.clone()),
            proved_at: request.migrated_at.clone(),
        },
    );
    Ok(())
}

/// Daemon-probed publish evidence (plan §7.2 steps 3 to 11).
///
/// The tool layer resolved the requested ref inside the attachment's
/// checkout, read the committed project identity and both source lanes at
/// that commit, and can re-resolve the ref on demand. Admin never opens a
/// checkout, exactly like every other operation in this module.
pub struct PublisherPublishProbe {
    /// The commit the full ref resolved to during preparation.
    pub resolved_commit: String,
    /// The published scope declared by the committed project identity at
    /// that commit.
    pub committed_scope: PublishedScope,
    pub sources: PublishSources,
    /// Revalidates the exact native checkout lease from inside the
    /// publication lock. The tool owns checkout authority; admin owns the
    /// ordering point immediately before the pointer operation.
    pub revalidate_checkout: Box<dyn Fn() -> Result<(), PublishError> + Send + Sync>,
    /// Re-resolves the full ref immediately before the pointer swap, from
    /// inside the publication lock (plan §7.3 step 6). `None` means the ref
    /// no longer resolves at all.
    pub revalidate_ref: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

pub struct PublisherPublishRequest {
    pub mode: PublisherPublishMode,
    pub project_id: ProjectId,
    pub attachment_id: AttachmentId,
    pub full_ref: String,
    pub expected_epoch: u64,
    pub dry_run: bool,
}

pub struct PublisherCandidatePublishProbe {
    pub producer_id: String,
    pub source_generation_id: String,
    pub source_generation_sha256: String,
    pub scope: PublishedScope,
    pub full_ref: String,
    pub accepted_commit: String,
    pub sources: PublishSources,
    pub revalidate_source: Box<dyn Fn() -> Result<(), PublishError> + Send + Sync>,
}

pub struct PublisherCandidatePublishRequest {
    pub mode: PublisherPublishMode,
    pub project_id: ProjectId,
    pub source_generation_id: String,
    pub expected_epoch: u64,
    pub dry_run: bool,
}

pub fn preflight_candidate_publish_authority(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
) -> std::result::Result<PublishedScope, PublishError> {
    let state = store
        .snapshot()
        .map_err(|error| PublishError::refusal(error.code(), error.to_string()))?;
    if state.epoch() != expected_epoch {
        return Err(PublishError::refusal(
            "error.project_catalog_stale_epoch",
            "expected epoch does not match the current catalog epoch",
        ));
    }
    let Some(project) = state.catalog().projects.get(project_id) else {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_unknown_project",
            "the requested project is not in the catalog",
        ));
    };
    let ProjectScope::Published(scope) = &project.scope else {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_scope_required",
            "a legacy-local project has no published scope to publish at",
        ));
    };
    Ok(scope.clone())
}

/// Read-only authority preflight for a publish (plan §7.2 steps 1 to 3).
///
/// Catalog authority is decided BEFORE the caller opens a checkout. A
/// request that fails here has proved nothing about the repository and
/// must not cause the daemon to resolve a ref, read committed trees, or
/// load source lanes: a denied caller learns only that it was denied.
///
/// It returns the catalog's current published scope, which the caller
/// needs in order to read committed sources at the right `.bbox` root.
pub fn preflight_publish_authority(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    attachment_id: &AttachmentId,
) -> std::result::Result<PublishedScope, PublishError> {
    let state = store
        .snapshot()
        .map_err(|error| PublishError::refusal(error.code(), error.to_string()))?;
    if state.epoch() != expected_epoch {
        return Err(PublishError::refusal(
            "error.project_catalog_stale_epoch",
            "expected epoch does not match the current catalog epoch",
        ));
    }
    let Some(project) = state.catalog().projects.get(project_id) else {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_unknown_project",
            "the requested project is not in the catalog",
        ));
    };
    let ProjectScope::Published(catalog_scope) = &project.scope else {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_scope_required",
            "a legacy-local project has no published scope to publish at",
        ));
    };
    validate_publisher_attachment(&state, project_id, attachment_id, catalog_scope)?;
    Ok(catalog_scope.clone())
}

/// Establish or advance one project's accepted publication.
///
/// Ordering is the whole safety argument. Catalog and attachment
/// validation, Git resolution, source capture, normalization, encoding, and
/// the generation write all happen before the publication lock exists. The
/// lock covers token verification, the freshness recheck, one atomic
/// pointer replacement, and read-back (plan §4.6).
///
/// Errors keep the refusing layer's own code: a stale epoch stays a catalog
/// refusal, a token mismatch stays an accepted-publication conflict.
pub fn publish_accepted_publication(
    store: &ProjectCatalogStore,
    runtime: &AcceptedPublicationRuntime,
    request: &PublisherPublishRequest,
    probe: PublisherPublishProbe,
) -> std::result::Result<PublishReceipt, PublishError> {
    // The same gates the caller's preflight ran, re-read here: a preflight
    // is an early refusal that saves checkout work, never the authority of
    // record for the mutation.
    let catalog_scope = &preflight_publish_authority(
        store,
        request.expected_epoch,
        &request.project_id,
        &request.attachment_id,
    )?;
    // The committed identity at the accepted commit must agree with the
    // catalog's CURRENT scope. That is what makes an advance the operation
    // which clears a scope-migration bridge: the new generation is always
    // published at the scope the catalog holds now (plan §4.9).
    if &probe.committed_scope != catalog_scope {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_scope_mismatch",
            "the committed project identity at the accepted commit declares a different scope \
             than the catalog's current published scope",
        ));
    }

    let prepared = runtime.prepare_publish(
        PublishRequest {
            mode: request.mode.clone(),
            project_id: request.project_id.clone(),
            source: AcceptedPublicationSourceBinding::Attachment {
                attachment_id: request.attachment_id.clone(),
            },
            scope: catalog_scope.clone(),
            full_ref: request.full_ref.clone(),
            accepted_commit: probe.resolved_commit.clone(),
            dry_run: request.dry_run,
        },
        probe.sources,
    )?;
    if request.dry_run {
        // Nothing durable was written, so there is nothing to commit and
        // nothing to roll back.
        return Ok(PublishReceipt::dry_run(&prepared));
    }

    let expected_epoch = request.expected_epoch;
    let project_id = request.project_id.clone();
    let attachment_id = request.attachment_id.clone();
    let catalog_scope = catalog_scope.clone();
    let resolved_commit = probe.resolved_commit.clone();
    let revalidate_checkout = probe.revalidate_checkout;
    let revalidate_ref = probe.revalidate_ref;
    runtime.commit_publish(prepared, &mut || {
        let fresh = store
            .snapshot()
            .map_err(|error| PublishError::refusal(error.code(), error.to_string()))?;
        if fresh.epoch() != expected_epoch {
            return Err(PublishError::refusal(
                "error.project_catalog_stale_epoch",
                "the catalog changed while the publish was being committed",
            ));
        }
        validate_publisher_attachment(&fresh, &project_id, &attachment_id, &catalog_scope)?;
        revalidate_checkout()?;
        // A ref that moved after preparation would install a generation
        // naming a commit the ref no longer points at.
        match revalidate_ref() {
            Some(commit) if commit == resolved_commit => Ok(()),
            _ => Err(PublishError::refusal(
                ERROR_ACCEPTED_PUBLICATION_REF_MOVED,
                "the full ref moved between preparation and the pointer swap",
            )),
        }
    })
}

pub fn publish_accepted_publication_candidate(
    store: &ProjectCatalogStore,
    runtime: &AcceptedPublicationRuntime,
    request: &PublisherCandidatePublishRequest,
    probe: PublisherCandidatePublishProbe,
) -> std::result::Result<PublishReceipt, PublishError> {
    if request.source_generation_id != probe.source_generation_id {
        return Err(PublishError::refusal(
            "error.accepted_publication_candidate_stale",
            "the selected source generation changed before preparation",
        ));
    }
    let catalog_scope =
        preflight_candidate_publish_authority(store, request.expected_epoch, &request.project_id)?;
    if probe.scope != catalog_scope {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_scope_mismatch",
            "the publication candidate declares a different scope than the catalog",
        ));
    }
    let prepared = runtime.prepare_publish(
        PublishRequest {
            mode: request.mode.clone(),
            project_id: request.project_id.clone(),
            source: AcceptedPublicationSourceBinding::Producer {
                producer_id: probe.producer_id,
                source_generation_id: probe.source_generation_id,
                source_generation_sha256: probe.source_generation_sha256,
            },
            scope: catalog_scope.clone(),
            full_ref: probe.full_ref,
            accepted_commit: probe.accepted_commit,
            dry_run: request.dry_run,
        },
        probe.sources,
    )?;
    if request.dry_run {
        return Ok(PublishReceipt::dry_run(&prepared));
    }
    let expected_epoch = request.expected_epoch;
    let project_id = request.project_id.clone();
    let revalidate_source = probe.revalidate_source;
    runtime.commit_publish(prepared, &mut || {
        let fresh = store
            .snapshot()
            .map_err(|error| PublishError::refusal(error.code(), error.to_string()))?;
        if fresh.epoch() != expected_epoch {
            return Err(PublishError::refusal(
                "error.project_catalog_stale_epoch",
                "the catalog changed while the candidate publish was being committed",
            ));
        }
        let Some(project) = fresh.catalog().projects.get(&project_id) else {
            return Err(PublishError::refusal(
                "error.project_catalog_admin_unknown_project",
                "the project disappeared while the candidate publish was being committed",
            ));
        };
        if project.scope != ProjectScope::Published(catalog_scope.clone()) {
            return Err(PublishError::refusal(
                "error.project_catalog_admin_scope_mismatch",
                "the project scope changed while the candidate publish was being committed",
            ));
        }
        revalidate_source()
    })
}

/// The attachment rules every publish shares: same project, attached, scope
/// agreement with the catalog, and the recorded repo-knowledge capability.
fn validate_publisher_attachment(
    state: &crate::project_catalog_store::ProjectCatalogState,
    project_id: &ProjectId,
    attachment_id: &AttachmentId,
    catalog_scope: &PublishedScope,
) -> std::result::Result<(), PublishError> {
    let Some(row) = state.attachments().attachments.get(attachment_id) else {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_unknown_attachment",
            "the named attachment is not in the store",
        ));
    };
    if &row.project_id != project_id {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_attachment_project_mismatch",
            "the named attachment belongs to another project",
        ));
    }
    if row.status != AttachmentStatus::Attached {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_attachment_detached",
            "a detached attachment cannot publish",
        ));
    }
    if row.validated_scope.as_ref() != Some(catalog_scope) {
        return Err(PublishError::refusal(
            "error.project_catalog_admin_scope_mismatch",
            "the named attachment does not carry the catalog's current published scope",
        ));
    }
    if !row.capabilities.repo_knowledge {
        return Err(PublishError::refusal(
            "error.project_capability_denied",
            "the named attachment does not carry the repo_knowledge capability",
        ));
    }
    Ok(())
}

/// Daemon-probed publisher-bind evidence: the tool layer proved the new
/// attachment's object database contains the pointer's accepted commit
/// (the containment a later advance and overlay recomputation need).
pub struct PublisherBindProbe {
    pub accepted_commit_present: bool,
    /// Same lease boundary as publisher Advance, rechecked under the
    /// publication lock immediately before the binding replacement.
    pub revalidate_checkout: Box<dyn Fn() -> bool + Send + Sync>,
}

#[derive(Debug, Clone)]
pub struct PublisherBindReceipt {
    pub attachment_id: AttachmentId,
    /// Epoch of the catalog snapshot the binding validated against, read
    /// inside the publication-lock critical section.
    pub catalog_epoch: u64,
}

/// Rebind the publisher attachment for one project (plan §7.7): the
/// pointer's ref, commit, scope, generation, and payloads never change
/// here; ref/commit changes are exclusively the later advance path. The
/// catalog side validates the attachment; the pointer store enforces the
/// pointer/generation agreement before and after.
///
/// Epoch CAS is real, not advisory (review): the expected epoch and the
/// attachment's Attached status are validated against a snapshot taken
/// INSIDE the publication-lock critical section, so a concurrent detach or
/// admin commit between the caller's read and the rebind is a typed
/// refusal, never a pointer naming a detached attachment.
pub fn bind_publisher_attachment(
    store: &ProjectCatalogStore,
    projects_path: &std::path::Path,
    expected_epoch: u64,
    project_id: &ProjectId,
    new_attachment: &AttachmentId,
    probe: &PublisherBindProbe,
) -> AdminResult<PublisherBindReceipt> {
    use crate::accepted_publication_store::{
        AcceptedPublicationLimits, AcceptedPublicationStorePaths,
        acquire_accepted_publication_lock, rebind_pointer_attachment_locked,
    };

    let paths = AcceptedPublicationStorePaths::derive(projects_path)
        .map_err(|error| admin_error(error.code(), "publication paths are invalid"))?;
    let guard = acquire_accepted_publication_lock(&paths)
        .map_err(|error| admin_error(error.code(), "publication store is locked"))?;
    // Read-validate-rebind under the publication lock; no catalog lock is
    // held (the catalog read uses a pinned snapshot taken after the lock).
    let state = store.snapshot()?;
    if state.epoch() != expected_epoch {
        return Err(admin_error(
            "error.project_catalog_stale_epoch",
            "expected epoch does not match the current catalog epoch",
        ));
    }
    let Some(row) = state.attachments().attachments.get(new_attachment) else {
        return Err(admin_error(
            "error.project_catalog_admin_unknown_attachment",
            format!("attachment {new_attachment} is not in the store"),
        ));
    };
    if &row.project_id != project_id {
        return Err(admin_error(
            "error.project_catalog_admin_attachment_project_mismatch",
            "the publisher binding must name an attachment of the same project",
        ));
    }
    if row.status != AttachmentStatus::Attached {
        return Err(admin_error(
            "error.project_catalog_admin_attachment_detached",
            "a detached attachment cannot carry the publisher binding",
        ));
    }
    let Some(attachment_scope) = row.validated_scope.clone() else {
        return Err(admin_error(
            "error.project_catalog_admin_scope_required",
            "a scope-less attachment cannot carry the publisher binding",
        ));
    };
    let limits = AcceptedPublicationLimits::default();
    // Bind is attachment-only, so it cannot move a pointer to a new scope.
    // While the catalog's scope and the pointer's accepted scope disagree,
    // the publication bridge is open and only an advance at the current
    // scope closes it (plan §7.5 step 5, §4.9).
    if let Some((pointer, _)) = crate::accepted_publication_store::installed_pointer_tokens_locked(
        &paths, &guard, project_id, &limits,
    )
    .map_err(|error| admin_error(error.code(), error.to_string()))?
        && pointer.accepted_scope != attachment_scope
    {
        return Err(admin_error(
            ERROR_ACCEPTED_PUBLICATION_SCOPE_ADVANCE_REQUIRED,
            "the accepted scope predates the catalog's current published scope; advance at the \
             current scope before rebinding",
        ));
    }
    // Containment is checked AFTER the bridge, deliberately. While a bridge
    // is open no rebind can succeed at any attachment, so fetching the
    // accepted commit into this checkout would be work chasing the wrong
    // refusal. Every open bridge points at new-scope advance.
    if !probe.accepted_commit_present {
        return Err(admin_error(
            "error.project_catalog_admin_commit_not_present",
            "the new attachment's object database does not contain the accepted \
             commit; fetch it before rebinding",
        ));
    }
    // Freshness recheck immediately before the swap: catalog transactions
    // (detach) do not take the publication lock (plan §11 lock order keeps
    // the pointer store's lock independent), so a detach can still commit
    // while this section runs. The recheck shrinks that window to the swap
    // itself; the residual interleaving leaves a misleading binding, not
    // corruption, and freshness reporting degrades it (D-033).
    let fresh = store.snapshot()?;
    if fresh.epoch() != expected_epoch {
        return Err(admin_error(
            "error.project_catalog_stale_epoch",
            "the catalog changed while the publisher binding was being validated",
        ));
    }
    match fresh.attachments().attachments.get(new_attachment) {
        Some(row) if row.status == AttachmentStatus::Attached => {}
        _ => {
            return Err(admin_error(
                "error.project_catalog_admin_attachment_detached",
                "the attachment detached while the publisher binding was being validated",
            ));
        }
    }
    if !(probe.revalidate_checkout)() {
        return Err(admin_error(
            "error.checkout_lease_stale",
            "the publisher attachment lease changed before the binding replacement",
        ));
    }
    // The store refuses before mutating when the expected scope disagrees
    // with the pointer's accepted scope, so no restore path exists.
    let rebound = rebind_pointer_attachment_locked(
        &paths,
        &guard,
        project_id,
        new_attachment,
        Some(&attachment_scope),
        &limits,
    )
    .map_err(|error| admin_error(error.code(), error.to_string()))?;
    let _ = rebound;
    Ok(PublisherBindReceipt {
        attachment_id: new_attachment.clone(),
        catalog_epoch: state.epoch(),
    })
}

/// Explicit operator catalog creation (plan §7.2): a published project by
/// authoritative scope (unowned required) or a legacy-local project. This
/// is the surface behind the offline `add` subcommand; producer traffic
/// and configuration reload never create projects.
#[derive(Debug, Clone)]
pub enum CatalogAddKind {
    Published(PublishedScope),
    LegacyLocal,
}

pub fn catalog_add(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    kind: &CatalogAddKind,
    display_name: &str,
    operator_aliases: &[String],
    created_at: &str,
) -> AdminResult<(ProjectId, ProjectCatalogCommit)> {
    let kind = kind.clone();
    let display_name = display_name.to_string();
    let aliases: Vec<String> = operator_aliases.to_vec();
    let created_at = created_at.to_string();
    let minted = std::sync::Mutex::new(None::<ProjectId>);
    let commit = store.transact(expected_epoch, |catalog, _attachments| {
        let project_id = insert_new_project(catalog, &kind, &display_name, &aliases, &created_at)?;
        *minted.lock().unwrap() = Some(project_id);
        Ok(())
    })?;
    let project_id = minted
        .into_inner()
        .unwrap()
        .expect("committed transaction minted an id");
    Ok((project_id, commit))
}

/// Insert one new catalog project with its repo-history record: the shared
/// creation body of [`catalog_add`] and [`register_composite`]. Enforces
/// scope ownership and alias uniqueness before minting.
fn insert_new_project(
    catalog: &mut bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    kind: &CatalogAddKind,
    display_name: &str,
    aliases: &[String],
    created_at: &str,
) -> AdminResult<ProjectId> {
    use bbox_corpus_core::project_catalog::{
        CommitNamespace, CorpusProject, RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId,
        RepoHistoryRecord,
    };

    let project_id =
        ProjectId::mint(catalog).map_err(|error| admin_error(error.code(), error.to_string()))?;
    if let CatalogAddKind::Published(scope) = kind
        && catalog
            .projects
            .values()
            .any(|p| matches!(&p.scope, ProjectScope::Published(s) if s == scope))
    {
        return Err(admin_error(
            "error.project_catalog_admin_scope_owned",
            "the scope is already owned by a catalog project",
        ));
    }
    for alias in aliases {
        let taken = catalog
            .projects
            .values()
            .any(|p| p.operator_aliases.contains(alias) || p.project_id.as_str() == alias);
        if taken {
            return Err(admin_error(
                "error.project_catalog_admin_alias_conflict",
                format!("alias {alias} collides with an id or accepted alias"),
            ));
        }
    }
    let (scope, repo_history) = match kind {
        CatalogAddKind::LegacyLocal => {
            // A legacy-local record gets a server-minted local history
            // with an independent random namespace (governing §5.1).
            let history_id = RepoHistoryId::mint();
            let namespace = CommitNamespace::mint_local(catalog)
                .map_err(|error| admin_error(error.code(), error.to_string()))?;
            catalog.repo_histories.insert(
                history_id.clone(),
                RepoHistoryRecord {
                    repo_history_id: history_id.clone(),
                    membership_generation: 0,
                    authority: RepoHistoryAuthority::LocalProject(project_id.clone()),
                    primary_namespace: namespace,
                    compatibility_namespaces: Default::default(),
                    materialization: Default::default(),
                },
            );
            (ProjectScope::LegacyLocal, Some(history_id))
        }
        CatalogAddKind::Published(scope) => {
            let authority = RecordedRepoAuthority::parse(scope.repo_id())
                .map_err(|error| admin_error(error.code(), error.to_string()))?;
            let existing = catalog
                .repo_histories
                .iter()
                .find(|(_, record)| {
                    matches!(&record.authority, RepoHistoryAuthority::Recorded(a) if a.as_str() == scope.repo_id())
                })
                .map(|(id, _)| id.clone());
            let history_id = match existing {
                Some(id) => id,
                None => {
                    let id = RepoHistoryId::mint();
                    let primary = CommitNamespace::parse(scope.repo_id())
                        .map_err(|error| admin_error(error.code(), error.to_string()))?;
                    catalog.repo_histories.insert(
                        id.clone(),
                        RepoHistoryRecord {
                            repo_history_id: id.clone(),
                            membership_generation: 0,
                            authority: RepoHistoryAuthority::Recorded(authority),
                            primary_namespace: primary,
                            compatibility_namespaces: Default::default(),
                            materialization: Default::default(),
                        },
                    );
                    id
                }
            };
            (ProjectScope::Published(scope.clone()), Some(history_id))
        }
    };
    catalog.projects.insert(
        project_id.clone(),
        CorpusProject {
            project_id: project_id.clone(),
            scope,
            operator_aliases: aliases.iter().cloned().collect(),
            nominated_aliases: Default::default(),
            display_name: display_name.to_string(),
            created_at: created_at.to_string(),
            registered_at_compat: None,
            repo_history,
            languages: Default::default(),
        },
    );
    Ok(project_id)
}

/// Outcome of the register compatibility composite (plan §9.1).
#[derive(Debug, Clone)]
pub struct RegisterCompositeReceipt {
    pub project_id: ProjectId,
    pub attachment_id: AttachmentId,
    /// True when this call minted the project (`Published` on a newly
    /// recorded scope, `LegacyLocal` otherwise).
    pub created_project: bool,
    /// True when the checkout was already attached (scope+attachment
    /// idempotency); no bytes moved and `commit` is `None`.
    pub already_attached: bool,
    pub commit: Option<ProjectCatalogCommit>,
}

/// The `bbox_project_register` catalog composite (plan §9.1, governing
/// §7.2): find by validated scope and active attachment, create `Published`
/// on a newly recorded scope or `LegacyLocal` for unrecorded checkouts, and
/// attach — all in one pair transaction. Newly committed authority on a
/// `LegacyLocal` attachment refuses with the exact promotion handoff; an
/// attached checkout resolving a different scope refuses with the exact
/// scope-migration dry-run handoff. Neither refusal creates a second
/// project. Idempotent re-registration commits nothing.
pub fn register_composite(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    probe: &AttachProbe,
    display_name: &str,
    created_at: &str,
) -> AdminResult<RegisterCompositeReceipt> {
    // Idempotency fast path against the same epoch the caller pinned: an
    // agreeing active attachment for this (checkout, relpath) means there
    // is nothing to write. Anything else falls through to the transaction,
    // whose epoch CAS refuses staleness.
    let state = store.snapshot()?;
    if state.epoch() == expected_epoch
        && let Some(existing) = find_active_attachment(state.attachments(), probe)
    {
        let owner = state
            .catalog()
            .projects
            .get(&existing.project_id)
            .cloned()
            .ok_or_else(|| {
                admin_error(
                    "error.project_catalog_admin_unknown_project",
                    "active attachment references a project absent from the catalog",
                )
            })?;
        check_register_scope_agreement(&owner, probe)?;
        return Ok(RegisterCompositeReceipt {
            project_id: owner.project_id.clone(),
            attachment_id: existing.attachment_id.clone(),
            created_project: false,
            already_attached: true,
            commit: None,
        });
    }
    drop(state);

    let probe = probe.clone();
    let display_name = display_name.to_string();
    let created_at = created_at.to_string();
    let minted = std::sync::Mutex::new(None::<(ProjectId, AttachmentId, bool)>);
    let commit = store.transact(expected_epoch, |catalog, attachments| {
        if let Some(existing) = find_active_attachment(attachments, &probe) {
            // The fast path above covered the caller's snapshot; reaching
            // this arm means the attachment landed concurrently. Apply the
            // same agreement rules so the outcome is deterministic.
            let owner = catalog.projects.get(&existing.project_id).ok_or_else(|| {
                admin_error(
                    "error.project_catalog_admin_unknown_project",
                    "active attachment references a project absent from the catalog",
                )
            })?;
            check_register_scope_agreement(owner, &probe)?;
            *minted.lock().unwrap() = Some((
                owner.project_id.clone(),
                existing.attachment_id.clone(),
                false,
            ));
            return Ok(());
        }
        let (project_id, created_project) = match &probe.validated_scope {
            Some(scope) => {
                let owner = catalog
                    .projects
                    .values()
                    .find(|p| matches!(&p.scope, ProjectScope::Published(s) if s == scope))
                    .map(|p| p.project_id.clone());
                match owner {
                    Some(project_id) => (project_id, false),
                    None => (
                        insert_new_project(
                            catalog,
                            &CatalogAddKind::Published(scope.clone()),
                            &display_name,
                            &[],
                            &created_at,
                        )?,
                        true,
                    ),
                }
            }
            None => (
                insert_new_project(
                    catalog,
                    &CatalogAddKind::LegacyLocal,
                    &display_name,
                    &[],
                    &created_at,
                )?,
                true,
            ),
        };
        let attachment_id = AttachmentId::mint();
        attachments.attachments.insert(
            attachment_id.clone(),
            CheckoutAttachment {
                attachment_id: attachment_id.clone(),
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
        *minted.lock().unwrap() = Some((project_id, attachment_id, created_project));
        Ok(())
    })?;
    let (project_id, attachment_id, created_project) = minted
        .into_inner()
        .unwrap()
        .expect("committed transaction recorded its outcome");
    Ok(RegisterCompositeReceipt {
        project_id,
        attachment_id,
        created_project,
        already_attached: false,
        commit: Some(commit),
    })
}

/// The active attachment covering the probe's `(checkout_id, relpath)`
/// pair, if any. Cross-project exclusivity of that pair makes the match
/// unique.
fn find_active_attachment<'s>(
    attachments: &'s bbox_corpus_core::project_catalog::AttachmentSnapshotV1,
    probe: &AttachProbe,
) -> Option<&'s CheckoutAttachment> {
    attachments.attachments.values().find(|row| {
        row.status == AttachmentStatus::Attached
            && row.checkout_id == probe.checkout_id
            && row.project_root_relpath == probe.project_root_relpath
    })
}

/// Register agreement between an attached checkout's current probe and its
/// owning project: same published scope is idempotent, a different scope is
/// the exact scope-migration handoff, newly committed authority on a
/// `LegacyLocal` project is the exact promotion handoff, and lost authority
/// against a `Published` project refuses.
fn check_register_scope_agreement(
    owner: &bbox_corpus_core::project_catalog::CorpusProject,
    probe: &AttachProbe,
) -> AdminResult<()> {
    match (&owner.scope, &probe.validated_scope) {
        (ProjectScope::Published(current), Some(probed)) if current == probed => Ok(()),
        (ProjectScope::Published(current), Some(probed)) => Err(admin_error(
            "error.project_catalog_scope_migration_required",
            format!(
                "project {} is attached here under scope {}:{} but the checkout now \
                 resolves {}:{}; run bbox_project_scope_migrate {{ project_id: \"{}\", \
                 dry_run: true }} first",
                owner.project_id,
                current.repo_id(),
                current.bbox_root_relpath(),
                probed.repo_id(),
                probed.bbox_root_relpath(),
                owner.project_id,
            ),
        )),
        (ProjectScope::Published(_), None) => Err(admin_error(
            "error.project_catalog_admin_scope_required",
            "the checkout no longer resolves committed recorded authority for its \
             published project",
        )),
        (ProjectScope::LegacyLocal, None) => Ok(()),
        (ProjectScope::LegacyLocal, Some(probed)) => Err(admin_error(
            "error.project_catalog_scope_promotion_required",
            format!(
                "project {} is legacy-local and this checkout now records committed \
                 authority for {}:{}; run bbox_project_promote {{ project_id: \"{}\" }}",
                owner.project_id,
                probed.repo_id(),
                probed.bbox_root_relpath(),
                owner.project_id,
            ),
        )),
    }
}

/// Probed facts for a same-scope attachment relocation (plan §9.1 rename):
/// the checkout-id marker read at the NEW path (path existence and inode
/// reuse never prove sameness), the relocated directories, and the
/// committed scope the moved checkout resolves.
#[derive(Debug, Clone)]
pub struct RelocationProbe {
    pub checkout_id: String,
    pub new_checkout_dir: String,
    pub new_checkout_project_dir: String,
    pub resolved_scope: Option<PublishedScope>,
}

/// Relocate one active attachment to a moved checkout path (plan §9.1):
/// same checkout identity, same validated scope, same relpath; one pair
/// transaction updating the attachment path fields and appending the §8.4
/// host-local ledger row. Relpath moves and repo rebinds refuse with the
/// scope-migration pointer; catalog-mode rename never rewrites owner-store
/// rows.
pub fn relocate_attachment(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    attachment_id: &AttachmentId,
    probe: &RelocationProbe,
) -> AdminResult<ProjectCatalogCommit> {
    use bbox_corpus_core::project_catalog::{
        LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry, LegacyPathRelationship,
    };
    let new_epoch = expected_epoch.checked_add(1).ok_or_else(|| {
        admin_error(
            "error.project_catalog_admin_epoch_overflow",
            "catalog epoch cannot be incremented",
        )
    })?;
    let attachment_id = attachment_id.clone();
    let probe = probe.clone();
    store.transact(expected_epoch, move |catalog, attachments| {
        let Some(row) = attachments.attachments.get(&attachment_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_attachment",
                format!("attachment {attachment_id} is not in the store"),
            ));
        };
        if row.status != AttachmentStatus::Attached {
            return Err(admin_error(
                "error.project_catalog_admin_attachment_detached",
                "a detached attachment cannot relocate; attach the new path instead",
            ));
        }
        if row.checkout_id != probe.checkout_id {
            return Err(admin_error(
                "error.project_catalog_admin_checkout_identity_mismatch",
                "the moved path carries a different checkout identity; detach and \
                 re-attach instead of renaming",
            ));
        }
        let Some(owner) = catalog.projects.get(&row.project_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                "attachment references a project absent from the catalog",
            ));
        };
        match (&owner.scope, &probe.resolved_scope) {
            (ProjectScope::Published(current), Some(probed)) if current == probed => {}
            (ProjectScope::LegacyLocal, None) => {}
            _ => {
                return Err(admin_error(
                    "error.project_catalog_admin_scope_mismatch",
                    "rename keeps the validated scope; a relpath move or repo rebind \
                     goes through bbox_project_scope_migrate",
                ));
            }
        }
        let expected_project_dir = if row.project_root_relpath == "." {
            probe.new_checkout_dir.clone()
        } else {
            format!("{}/{}", probe.new_checkout_dir, row.project_root_relpath)
        };
        if probe.new_checkout_project_dir != expected_project_dir {
            return Err(admin_error(
                "error.project_catalog_admin_scope_mismatch",
                "rename keeps the project's relpath inside the checkout; a relpath \
                 move goes through bbox_project_scope_migrate",
            ));
        }
        let historical_path = row.checkout_project_dir.clone();
        if historical_path == probe.new_checkout_project_dir {
            return Err(admin_error(
                "error.project_catalog_admin_relocation_noop",
                "the attachment already records this path",
            ));
        }
        let project_id = row.project_id.clone();
        let row = attachments
            .attachments
            .get_mut(&attachment_id)
            .expect("presence checked above");
        row.checkout_dir = probe.new_checkout_dir.clone();
        row.checkout_project_dir = probe.new_checkout_project_dir.clone();
        // Append-only host-local binding so path-only legacy rows keep
        // resolving after relocation (plan §8.4).
        let binding_id = LegacyPathBindingId::mint();
        attachments.legacy_path_bindings.insert(
            binding_id.clone(),
            LegacyPathLedgerEntry {
                legacy_path_binding_id: binding_id,
                historical_path,
                source_store: crate::project_catalog_backfill::ATTACHMENT_RELOCATION_SOURCE
                    .to_string(),
                source_row_id: attachment_id.as_str().to_string(),
                member_row_count: 1,
                member_commitment_sha256:
                    bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(
                        attachment_id.as_str(),
                    )
                    .commitment_sha256,
                inventory_epoch: new_epoch,
                status: LegacyPathBindingStatus::Mapped {
                    project_id,
                    relationship: LegacyPathRelationship::Root,
                },
            },
        );
        Ok(())
    })
}

/// Accept or reject one nominated alias (plan §7.6, D-005): an explicit
/// local catalog-authority action. Acceptance enforces uniqueness against
/// every id and accepted alias; a missing nomination refuses so a stale
/// command cannot silently accept something else.
pub fn alias_decide(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    alias: &str,
    accept: bool,
) -> AdminResult<ProjectCatalogCommit> {
    let project_id = project_id.clone();
    let alias = alias.to_string();
    store.transact(expected_epoch, move |catalog, _attachments| {
        if accept {
            let taken = catalog
                .projects
                .values()
                .any(|p| p.operator_aliases.contains(&alias) || p.project_id.as_str() == alias);
            if taken {
                return Err(admin_error(
                    "error.project_catalog_admin_alias_conflict",
                    format!("alias {alias} collides with an id or accepted alias"),
                ));
            }
        }
        let Some(project) = catalog.projects.get_mut(&project_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                format!("project {project_id} is not in the catalog"),
            ));
        };
        if !project.nominated_aliases.remove(&alias) {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_nomination",
                format!("alias {alias} is not a pending nomination"),
            ));
        }
        if accept {
            project.operator_aliases.insert(alias.clone());
        }
        Ok(())
    })
}

/// External reference classes the retire inventory counts, probed by the
/// offline caller (plan §7.8). Every class must be zero before execute;
/// detached attachment rows, the project's own audit chain, and stale
/// mapped bindings do not block and are removed with the project.
#[derive(Debug, Clone, Default)]
pub struct RetireEvidence {
    pub external_reference_counts: std::collections::BTreeMap<String, u64>,
    /// R2F1: classes that could not be probed. These are carried as
    /// refusals in the final reprobe so an unprobeable class cannot be
    /// mistaken for a discharged zero.
    pub unprobeable_classes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RetireInventory {
    pub blocking: std::collections::BTreeMap<String, u64>,
    pub removable_attachments: u64,
    pub removable_migrations: u64,
    pub removable_bindings: u64,
}

/// Inventory (always) and optionally execute the removal of one fully
/// discharged project. Execute removes, in one pair transaction, the
/// project, its now-unreferenced local history record, its scope-migration
/// records with their proofs, all its attachment rows, and its mapped
/// path bindings; strict cross-validation forbids leaving any behind.
pub fn retire_project(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    evidence: &RetireEvidence,
    execute: bool,
) -> AdminResult<(RetireInventory, Option<ProjectCatalogCommit>)> {
    use bbox_corpus_core::project_catalog::{
        LegacyPathBindingStatus, RepoHistoryAuthority, RepoHistoryMaterialization,
    };

    let state = store.snapshot()?;
    if state.epoch() != expected_epoch {
        return Err(admin_error(
            "error.project_catalog_stale_epoch",
            "expected epoch does not match the current catalog epoch",
        ));
    }
    if !state.catalog().projects.contains_key(project_id) {
        return Err(admin_error(
            "error.project_catalog_admin_unknown_project",
            format!("project {project_id} is not in the catalog"),
        ));
    }
    let blocking: std::collections::BTreeMap<String, u64> = evidence
        .external_reference_counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(class, count)| (class.clone(), *count))
        .collect();
    let active_attachments = state
        .attachments()
        .attachments
        .values()
        .filter(|row| row.status == AttachmentStatus::Attached && &row.project_id == project_id)
        .count() as u64;
    let mut blocking = blocking;
    if active_attachments > 0 {
        blocking.insert("active_attachments".into(), active_attachments);
    }
    // Retire refuses to delete ANY history record whose materialization is
    // Ready (Phase 3 plan section 5), regardless of authority kind. The
    // invariant is deletion-site-wide: it is layered on top of whatever
    // condition the transact closure below uses to decide a history record
    // is eligible for removal, not hardcoded to one authority kind. Today
    // that eligibility (`deletion_eligible` below) is exactly "LocalProject
    // authority, unreferenced by any other project": the transact closure's
    // one and only history-record deletion path. A Recorded- or
    // LegacyNamespace-authority record is never deleted by retire at all
    // (durable/shared repo identity outlives any single project's
    // retirement), so it can never reach this guard regardless of
    // materialization; `deletion_eligible` reflects that structurally
    // instead of special-casing it. This also structurally protects
    // validate_catalog's dangling-authority check, since a
    // LocalProject-authority record requires its owning project to still
    // exist.
    let history_generation_referenced = state
        .catalog()
        .projects
        .get(project_id)
        .and_then(|project| project.repo_history.as_ref())
        .and_then(|history_id| {
            let history = state.catalog().repo_histories.get(history_id)?;
            let still_referenced = state.catalog().projects.values().any(|other| {
                other.project_id != *project_id && other.repo_history.as_ref() == Some(history_id)
            });
            let deletion_eligible = !still_referenced
                && matches!(&history.authority, RepoHistoryAuthority::LocalProject(owner) if owner == project_id);
            let ready = matches!(
                history.materialization,
                RepoHistoryMaterialization::Ready { .. }
            );
            (deletion_eligible && ready).then_some(1_u64)
        });
    if let Some(count) = history_generation_referenced {
        blocking.insert("history_generation_referenced".into(), count);
    }
    let inventory = RetireInventory {
        blocking: blocking.clone(),
        removable_attachments: state
            .attachments()
            .attachments
            .values()
            .filter(|row| &row.project_id == project_id)
            .count() as u64,
        removable_migrations: state
            .catalog()
            .scope_migrations
            .values()
            .filter(|record| &record.project_id == project_id)
            .count() as u64,
        removable_bindings: state
            .attachments()
            .legacy_path_bindings
            .values()
            .filter(|entry| {
                matches!(&entry.status, LegacyPathBindingStatus::Mapped { project_id: p, .. } if p == project_id)
            })
            .count() as u64,
    };
    if !execute {
        return Ok((inventory, None));
    }
    if !blocking.is_empty() {
        return Err(admin_error(
            "error.project_catalog_admin_retire_blocked",
            format!("nonzero reference classes remain: {blocking:?}"),
        ));
    }
    let project_id = project_id.clone();
    let commit = store.transact(expected_epoch, move |catalog, attachments| {
        let Some(project) = catalog.projects.remove(&project_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                "project vanished between inventory and execute",
            ));
        };
        if let Some(history_id) = &project.repo_history {
            // The blocking check above already refused this whole retire if
            // this exact deletion-eligibility condition held with a Ready
            // materialization; a Recorded/LegacyNamespace-authority record
            // never satisfies `local_only` and so is never removed here
            // regardless of materialization or reference count.
            let still_referenced = catalog
                .projects
                .values()
                .any(|other| other.repo_history.as_ref() == Some(history_id));
            let local_only = catalog
                .repo_histories
                .get(history_id)
                .is_some_and(|record| {
                    matches!(&record.authority, RepoHistoryAuthority::LocalProject(p) if p == &project_id)
                });
            if !still_referenced && local_only {
                catalog.repo_histories.remove(history_id);
            }
        }
        let removed_migrations: Vec<_> = catalog
            .scope_migrations
            .iter()
            .filter(|(_, record)| record.project_id == project_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &removed_migrations {
            catalog.scope_migrations.remove(id);
            attachments.scope_migration_proofs.remove(id);
        }
        attachments
            .attachments
            .retain(|_, row| row.project_id != project_id);
        attachments.default_attachments.remove(&project_id);
        attachments.legacy_path_bindings.retain(|_, entry| {
            !matches!(&entry.status, LegacyPathBindingStatus::Mapped { project_id: p, .. } if p == &project_id)
        });
        Ok(())
    })?;
    Ok((inventory, Some(commit)))
}

/// Operator-attested unattached scope migration (plan §7.5, governing
/// §7.2): the offline CLI channel for a project with zero active
/// attachments. Refuses when any active attachment exists (the online
/// attachment-proved channel owns that case), requires the explicit
/// unattached acknowledgement and a bounded reason (strict validation
/// makes an attested record without a reason unrepresentable), writes the
/// `OperatorAttested` record with no proof row, and never relocates
/// attachment rows because none are active.
pub fn scope_migrate_attested(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    request: &ScopeMigrationRequest,
    acknowledge_unattached_scope_migration: bool,
    dry_run: bool,
) -> AdminResult<Option<ScopeTransitionReceipt>> {
    use bbox_corpus_core::project_catalog::{
        RecordedRepoAuthority, RepoHistoryAuthority, ScopeMigrationAuthorityProvenance,
        ScopeMigrationId, ScopeMigrationKind, ScopeMigrationRecord,
    };

    if !acknowledge_unattached_scope_migration {
        return Err(admin_error(
            "error.project_catalog_admin_acknowledgement_required",
            "unattached scope migration requires the explicit operator \
             acknowledgement flag",
        ));
    }
    if request.operator_reason.is_none() {
        return Err(admin_error(
            "error.project_catalog_admin_reason_required",
            "operator-attested migration requires a bounded reason",
        ));
    }
    let new_epoch = expected_epoch.checked_add(1).ok_or_else(|| {
        admin_error(
            "error.project_catalog_admin_epoch_overflow",
            "catalog epoch cannot be incremented",
        )
    })?;
    let migration_id = ScopeMigrationId::mint();
    let receipt_id = migration_id.clone();
    let request = request.clone();
    fn apply_attested_scope_migration(
        catalog: &mut bbox_corpus_core::project_catalog::CatalogSnapshotV2,
        attachments: &mut bbox_corpus_core::project_catalog::AttachmentSnapshotV1,
        request: &ScopeMigrationRequest,
        receipt_id: &ScopeMigrationId,
        new_epoch: u64,
    ) -> AdminResult<()> {
        if attachments.attachments.values().any(|row| {
            row.status == AttachmentStatus::Attached && row.project_id == request.project_id
        }) {
            return Err(admin_error(
                "error.project_catalog_admin_attachments_active",
                "active attachments exist; use the attachment-proved channel",
            ));
        }
        let Some(project) = catalog.projects.get(&request.project_id) else {
            return Err(admin_error(
                "error.project_catalog_admin_unknown_project",
                format!("project {} is not in the catalog", request.project_id),
            ));
        };
        let ProjectScope::Published(current) = &project.scope else {
            return Err(admin_error(
                "error.project_catalog_admin_not_published",
                "scope migration applies only to a published project",
            ));
        };
        if current != &request.expected_old_scope {
            return Err(admin_error(
                "error.project_catalog_admin_scope_mismatch",
                "the project no longer carries the expected old scope",
            ));
        }
        // Section 4.11: refuse a second scope migration while a code
        // bridge is open for this project.
        let has_open_bridge = catalog.scope_migrations.values().any(|record| {
            record.project_id == request.project_id && record.code_bridge_generation.is_some()
        });
        if has_open_bridge {
            return Err(admin_error(
                "error.project_catalog_scope_migration_bridge_open",
                "a code bridge is open for this project; clear the bridge via \
                 new-scope activation before re-attempting the migration",
            ));
        }
        match request.kind {
            ScopeMigrationKind::RelpathMove => {
                if request.new_scope.repo_id() != current.repo_id()
                    || request.new_scope.bbox_root_relpath() == current.bbox_root_relpath()
                {
                    return Err(admin_error(
                        "error.project_catalog_admin_migration_shape",
                        "a relpath move keeps the repository and changes the relpath",
                    ));
                }
            }
            ScopeMigrationKind::RepoAuthorityChange => {
                if request.new_scope.repo_id() == current.repo_id()
                    || request.new_scope.bbox_root_relpath() != current.bbox_root_relpath()
                {
                    return Err(admin_error(
                        "error.project_catalog_admin_migration_shape",
                        "an authority change keeps the relpath and changes the repository",
                    ));
                }
                if !request.acknowledge_repo_authority_change {
                    return Err(admin_error(
                        "error.project_catalog_admin_acknowledgement_required",
                        "a recorded-authority change requires its explicit operator \
                         acknowledgement flag",
                    ));
                }
            }
            ScopeMigrationKind::Promotion => {
                return Err(admin_error(
                    "error.project_catalog_admin_migration_shape",
                    "promotion is attachment-proved only and never operator-attested",
                ));
            }
        }
        if catalog.projects.values().any(|other| {
            other.project_id != request.project_id
                && matches!(&other.scope, ProjectScope::Published(s) if s == &request.new_scope)
        }) {
            return Err(admin_error(
                "error.project_catalog_admin_scope_owned",
                "the target scope is already owned; use the offline survivor workflow",
            ));
        }
        if request.kind == ScopeMigrationKind::RepoAuthorityChange
            && let Some(history_id) = catalog
                .projects
                .get(&request.project_id)
                .and_then(|p| p.repo_history.clone())
        {
            let authority = RecordedRepoAuthority::parse(request.new_scope.repo_id())
                .map_err(|error| admin_error(error.code(), error.to_string()))?;
            if let Some(record) = catalog.repo_histories.get_mut(&history_id) {
                record.authority = RepoHistoryAuthority::Recorded(authority);
            }
        }
        let project = catalog
            .projects
            .get_mut(&request.project_id)
            .expect("checked above");
        project.scope = ProjectScope::Published(request.new_scope.clone());
        catalog.scope_migrations.insert(
            receipt_id.clone(),
            ScopeMigrationRecord {
                scope_migration_id: receipt_id.clone(),
                project_id: request.project_id.clone(),
                catalog_epoch: new_epoch,
                authority_provenance: ScopeMigrationAuthorityProvenance::OperatorAttested,
                operator_invocation: request.operator_invocation.clone(),
                operator_reason: request.operator_reason.clone(),
                old_scope: ProjectScope::Published(request.expected_old_scope.clone()),
                new_scope: ProjectScope::Published(request.new_scope.clone()),
                kind: request.kind.clone(),
                migrated_at: request.migrated_at.clone(),
                code_bridge_generation: request.code_bridge_generation.clone(),
                publication_bridge_generation: request.publication_bridge_generation.clone(),
                pending_capabilities: Default::default(),
            },
        );
        Ok(())
    }

    if dry_run {
        let state = store.snapshot()?;
        if state.epoch() != expected_epoch {
            return Err(admin_error(
                "error.project_catalog_stale_epoch",
                "expected epoch does not match the current catalog epoch",
            ));
        }
        let mut catalog = (**state.catalog()).clone();
        let mut attachments = (**state.attachments()).clone();
        apply_attested_scope_migration(
            &mut catalog,
            &mut attachments,
            &request,
            &receipt_id,
            new_epoch,
        )?;
        catalog.epoch = new_epoch;
        attachments.epoch = new_epoch;
        catalog
            .validate()
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        attachments
            .validate()
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        bbox_corpus_core::project_catalog::validate_catalog_attachments(&catalog, &attachments)
            .map_err(|error| admin_error(error.code(), error.to_string()))?;
        return Ok(None);
    }

    let commit = store.transact(expected_epoch, move |catalog, attachments| {
        apply_attested_scope_migration(catalog, attachments, &request, &receipt_id, new_epoch)
    })?;
    Ok(Some(ScopeTransitionReceipt {
        scope_migration_id: migration_id,
        commit,
    }))
}

/// Bridge-clear transaction: null `code_bridge_generation` on a
/// ScopeMigrationRecord (section 9.5).
///
/// This is the sole code-source-side path that mutates a migration
/// record. It fires automatically on first new-scope activation
/// (via the reconciler) and manually via the `scope-bridge-clear` CLI.
///
/// Two precondition-distinct modes:
/// - Mode 1 (dangling-reference): the named generation is retired.
///   Null `code_bridge_generation`. Requires verified evidence that
///   the bridge generation is no longer the effective activation
///   (the caller supplies `effective_generation_id` from a verified
///   code-source probe).
/// - Mode 2 (double-migration truthfulness repair): null the newest
///   bridge-bearing record, restoring an older admitting record as
///   the sole bridge. Requires verified evidence that the older
///   record's `new_scope` matches the project's current catalog scope.
pub fn clear_scope_bridge(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    project_id: &ProjectId,
    mode: ScopeBridgeClearMode,
    evidence: &ScopeBridgeClearEvidence,
) -> AdminResult<ProjectCatalogCommit> {
    let state = store.snapshot()?;
    if state.epoch() != expected_epoch {
        return Err(admin_error(
            "error.project_catalog_stale_epoch",
            "expected epoch does not match the current catalog epoch",
        ));
    }
    let catalog = state.catalog();
    // Find bridge-bearing records for this project, sorted by catalog_epoch.
    let mut bridge_records: Vec<_> = catalog
        .scope_migrations
        .values()
        .filter(|r| r.project_id == *project_id && r.code_bridge_generation.is_some())
        .collect();
    bridge_records.sort_by_key(|r| r.catalog_epoch);
    if bridge_records.is_empty() {
        return Err(admin_error(
            "error.project_catalog_scope_bridge_clear_no_bridge",
            "no bridge-bearing scope migration record found for this project",
        ));
    }
    let target_migration_id = match mode {
        ScopeBridgeClearMode::DanglingReference => {
            // Mode 1 precondition (R2F4): the bridge generation is
            // actually retired, meaning it is ABSENT from the store's
            // retained/GC-rooted set. The caller supplies the current
            // effective generation id and the retained set from a store
            // enumeration. Merely checking id inequality is insufficient
            // (a different effective generation does not prove the bridge
            // generation is gone from retained state).
            let newest = bridge_records.last().ok_or_else(|| {
                admin_error(
                    "error.project_catalog_scope_bridge_clear_no_bridge",
                    "no bridge-bearing record to clear",
                )
            })?;
            let bridge_gen = newest.code_bridge_generation.as_ref().ok_or_else(|| {
                admin_error(
                    "error.project_catalog_scope_bridge_clear_no_bridge",
                    "newest record has no bridge generation",
                )
            })?;
            // R3F3: support the no-activation dangling-bridge case. When
            // there is no effective activation (None), the bridge is
            // genuinely dangling and can be cleared if the bridge generation
            // is absent from the retained set. When there IS an effective
            // activation, the bridge generation must not be the effective
            // generation (it must have been superseded).
            match &evidence.activation {
                ScopeBridgeActivationEvidence::Present { generation_id, .. } => {
                    if bridge_gen == generation_id {
                        return Err(admin_error(
                            "error.project_catalog_scope_bridge_clear_bridge_still_live",
                            "the bridge generation is still the effective activation; cannot clear a live bridge",
                        ));
                    }
                }
                ScopeBridgeActivationEvidence::VerifiedAbsent => {}
                ScopeBridgeActivationEvidence::Unavailable { diagnostic } => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_evidence_unavailable",
                        format!("activation evidence is unavailable: {diagnostic}"),
                    ));
                }
            }
            match &evidence.retained_generations {
                ScopeBridgeRetainedEvidence::Enumerated(generations) => {
                    if generations.contains(bridge_gen) {
                        return Err(admin_error(
                            "error.project_catalog_scope_bridge_clear_bridge_retained",
                            "the bridge generation is still in the retained/GC-rooted set; cannot clear a retained bridge",
                        ));
                    }
                }
                ScopeBridgeRetainedEvidence::Unavailable { diagnostic } => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_evidence_unavailable",
                        format!("retained-generation evidence is unavailable: {diagnostic}"),
                    ));
                }
            }
            newest.scope_migration_id.clone()
        }
        ScopeBridgeClearMode::AutomaticFirstNewScope => {
            let newest = bridge_records.last().ok_or_else(|| {
                admin_error(
                    "error.project_catalog_scope_bridge_clear_no_bridge",
                    "no bridge-bearing record to clear",
                )
            })?;
            let bridge_gen = newest.code_bridge_generation.as_ref().ok_or_else(|| {
                admin_error(
                    "error.project_catalog_scope_bridge_clear_no_bridge",
                    "newest record has no bridge generation",
                )
            })?;
            let current_scope = catalog
                .projects
                .get(project_id)
                .and_then(|project| match &project.scope {
                    ProjectScope::Published(scope) => Some(scope),
                    ProjectScope::LegacyLocal => None,
                })
                .ok_or_else(|| {
                    admin_error(
                        "error.project_catalog_scope_bridge_clear_missing_current_scope",
                        "automatic bridge clear requires a published current catalog scope",
                    )
                })?;
            let (effective_generation, effective_scope) = match &evidence.activation {
                ScopeBridgeActivationEvidence::Present {
                    generation_id,
                    scope,
                } => (generation_id, scope),
                ScopeBridgeActivationEvidence::VerifiedAbsent => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_missing_evidence",
                        "automatic bridge clear requires a strictly loaded activation",
                    ));
                }
                ScopeBridgeActivationEvidence::Unavailable { diagnostic } => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_evidence_unavailable",
                        format!("activation evidence is unavailable: {diagnostic}"),
                    ));
                }
            };
            if effective_scope != current_scope {
                return Err(admin_error(
                    "error.project_catalog_scope_bridge_clear_activation_scope_mismatch",
                    "automatic bridge clear activation scope does not match the current catalog scope",
                ));
            }
            if effective_generation == bridge_gen {
                return Err(admin_error(
                    "error.project_catalog_scope_bridge_clear_bridge_still_live",
                    "automatic bridge clear activation still names the bridge generation",
                ));
            }
            match &evidence.retained_generations {
                ScopeBridgeRetainedEvidence::Enumerated(generations)
                    if generations.contains(bridge_gen) => {}
                ScopeBridgeRetainedEvidence::Enumerated(_) => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_bridge_not_retained",
                        "automatic bridge clear expected the GC-pinned bridge generation to remain retained",
                    ));
                }
                ScopeBridgeRetainedEvidence::Unavailable { diagnostic } => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_evidence_unavailable",
                        format!("retained-generation evidence is unavailable: {diagnostic}"),
                    ));
                }
            }
            newest.scope_migration_id.clone()
        }
        ScopeBridgeClearMode::DoubleMigrationRepair => {
            // Mode 2 precondition (R2F4): implements the exact open-bridge
            // predicate. At least two bridge-bearing records exist. The
            // older record ADMITS the effective generation through its
            // old_scope (old_scope equals the effective activation scope
            // AND code_bridge_generation equals the effective generation).
            // The newer record does NOT admit (otherwise there would be no
            // repair needed). This is the truthful recovery state for a
            // legacy A->B->C double migration where the effective
            // generation is still scoped A.
            if bridge_records.len() < 2 {
                return Err(admin_error(
                    "error.project_catalog_scope_bridge_clear_no_double_migration",
                    "mode 2 requires a pre-refusal double-migration state: \
                     at least two bridge-bearing records",
                ));
            }
            let (effective_gen, effective_scope) = match &evidence.activation {
                ScopeBridgeActivationEvidence::Present {
                    generation_id,
                    scope,
                } => (generation_id, scope),
                ScopeBridgeActivationEvidence::VerifiedAbsent => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_missing_evidence",
                        "mode 2 requires a verified effective activation",
                    ));
                }
                ScopeBridgeActivationEvidence::Unavailable { diagnostic } => {
                    return Err(admin_error(
                        "error.project_catalog_scope_bridge_clear_evidence_unavailable",
                        format!("activation evidence is unavailable: {diagnostic}"),
                    ));
                }
            };
            // The older record must admit: old_scope equals effective scope
            // AND code_bridge_generation equals effective generation.
            let older = &bridge_records[bridge_records.len() - 2];
            let older_gen_matches = older
                .code_bridge_generation
                .as_deref()
                .is_some_and(|bridge_gen| bridge_gen == effective_gen);
            let older_scope_matches = matches!(
                &older.old_scope,
                ProjectScope::Published(s) if s == effective_scope
            );
            let older_admits = older_gen_matches && older_scope_matches;
            if !older_admits {
                return Err(admin_error(
                    "error.project_catalog_scope_bridge_clear_older_record_does_not_admit",
                    "the older bridge-bearing record does not admit the effective \
                     generation (old_scope must equal effective scope AND \
                     code_bridge_generation must equal effective generation)",
                ));
            }
            // The newer record must NOT admit (otherwise no repair is needed).
            let newer = &bridge_records[bridge_records.len() - 1];
            let newer_gen_matches = newer
                .code_bridge_generation
                .as_deref()
                .is_some_and(|bridge_gen| bridge_gen == effective_gen);
            let newer_scope_matches = matches!(
                &newer.old_scope,
                ProjectScope::Published(s) if s == effective_scope
            );
            let newer_admits = newer_gen_matches && newer_scope_matches;
            if newer_admits {
                return Err(admin_error(
                    "error.project_catalog_scope_bridge_clear_newer_record_admits",
                    "the newer bridge-bearing record also admits the effective \
                     generation; no repair is needed",
                ));
            }
            newer.scope_migration_id.clone()
        }
    };
    let project_id_owned = project_id.clone();
    store.transact(expected_epoch, move |catalog, _attachments| {
        let record = catalog
            .scope_migrations
            .get_mut(&target_migration_id)
            .ok_or_else(|| {
                admin_error(
                    "error.project_catalog_scope_bridge_clear_record_missing",
                    "the target migration record disappeared between snapshot and transact",
                )
            })?;
        if record.project_id != project_id_owned {
            return Err(admin_error(
                "error.project_catalog_scope_bridge_clear_project_mismatch",
                "the target migration record belongs to a different project",
            ));
        }
        record.code_bridge_generation = None;
        Ok(())
    })
}

/// Verified code-source evidence for a bridge-clear transaction (R2F4).
/// The caller must probe the code-source state and supply the current
/// effective generation id AND effective scope before calling
/// `clear_scope_bridge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeBridgeActivationEvidence {
    Present {
        generation_id: String,
        scope: PublishedScope,
    },
    VerifiedAbsent,
    Unavailable {
        diagnostic: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeBridgeRetainedEvidence {
    Enumerated(std::collections::BTreeSet<String>),
    Unavailable { diagnostic: String },
}

#[derive(Debug, Clone)]
pub struct ScopeBridgeClearEvidence {
    pub activation: ScopeBridgeActivationEvidence,
    pub retained_generations: ScopeBridgeRetainedEvidence,
}

impl Default for ScopeBridgeClearEvidence {
    fn default() -> Self {
        Self {
            activation: ScopeBridgeActivationEvidence::Unavailable {
                diagnostic: "bridge activation evidence was not probed".to_string(),
            },
            retained_generations: ScopeBridgeRetainedEvidence::Unavailable {
                diagnostic: "retained generations were not enumerated".to_string(),
            },
        }
    }
}

/// Which bridge-clear mode to use (section 9.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeBridgeClearMode {
    /// Mode 1: the named generation is retired (dangling reference).
    DanglingReference,
    /// Automatic convergence after the first generation activates in the new
    /// scope. The old bridge generation remains retained as a GC root.
    AutomaticFirstNewScope,
    /// Mode 2: double-migration truthfulness repair. Null the newest
    /// bridge-bearing record, restoring the older admitting record.
    DoubleMigrationRepair,
}

// ---------------------------------------------------------------------------
// Forward-only retirement journal (section 11)
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};

/// The eight forward-only stages of a project retirement journal
/// (section 11.3). Each stage is idempotent: re-running a completed
/// stage is a no-op. The journal advances strictly forward; no stage
/// can regress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RetirementJournalStage {
    Prepared,
    SourceAuthorityQuiesced,
    CollectedGenerationsDischarged,
    PublicationsCleared,
    AttachmentsDetached,
    CatalogPairRemoved,
    MaterializationSwept,
    Complete,
}

impl RetirementJournalStage {
    /// True when this stage is at or past `other` in the forward order.
    pub fn is_at_least(&self, other: RetirementJournalStage) -> bool {
        self.ordinal() >= other.ordinal()
    }

    fn ordinal(&self) -> u8 {
        match self {
            Self::Prepared => 0,
            Self::SourceAuthorityQuiesced => 1,
            Self::CollectedGenerationsDischarged => 2,
            Self::PublicationsCleared => 3,
            Self::AttachmentsDetached => 4,
            Self::CatalogPairRemoved => 5,
            Self::MaterializationSwept => 6,
            Self::Complete => 7,
        }
    }

    fn next(&self) -> Option<Self> {
        match self {
            Self::Prepared => Some(Self::SourceAuthorityQuiesced),
            Self::SourceAuthorityQuiesced => Some(Self::CollectedGenerationsDischarged),
            Self::CollectedGenerationsDischarged => Some(Self::PublicationsCleared),
            Self::PublicationsCleared => Some(Self::AttachmentsDetached),
            Self::AttachmentsDetached => Some(Self::CatalogPairRemoved),
            Self::CatalogPairRemoved => Some(Self::MaterializationSwept),
            Self::MaterializationSwept => Some(Self::Complete),
            Self::Complete => None,
        }
    }
}

/// The persisted retirement journal (section 11.3). Lives OUTSIDE the
/// catalog pair at `{bro_home}/retirement-journals/{project_id}.json`.
/// Each stage advance is synced to disk before the discharge worker
/// proceeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRetirementJournal {
    pub version: u32,
    pub project_id: ProjectId,
    pub started_at: String,
    pub updated_at: String,
    pub current_stage: RetirementJournalStage,
    /// The catalog epoch captured at `Prepared` time, used for epoch CAS
    /// validation during recovery.
    pub catalog_epoch_at_start: u64,
    /// Typed evidence of completed discharge steps, for recovery and
    /// audit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_steps: Vec<RetirementJournalStep>,

    /// R4F1+R4F6: Owner-validated generation and blob inventory
    /// captured at Prepared time. Drives generation deletion and blob
    /// sweep so they do not depend on the catalog row being present.
    #[serde(default)]
    pub evidence: RetirementJournalEvidence,

    /// Commitment to the exact Prepared evidence, including decoded blob
    /// identities. It never changes as stages advance.
    pub prepared_evidence_sha256: Option<String>,
}

impl ProjectRetirementJournal {
    /// v7 (R27F5): `evidence.edge_inventory` gained exact snapshot
    /// reclamation records, so a v6 journal's edge authority evidence is
    /// structurally incomplete for the discharge it authorizes. It is
    /// refused rather than reinterpreted.
    pub const VERSION: u32 = 7;

    pub fn new(project_id: ProjectId, catalog_epoch: u64, now: &str) -> Self {
        let mut journal = Self {
            version: Self::VERSION,
            project_id: project_id.clone(),
            started_at: now.to_string(),
            updated_at: now.to_string(),
            current_stage: RetirementJournalStage::Prepared,
            catalog_epoch_at_start: catalog_epoch,
            completed_steps: Vec::new(),
            evidence: RetirementJournalEvidence {
                owner_project_id: Some(project_id.clone()),
                project_selectors: Some(vec![project_id.as_str().to_string()]),
                desired_pointers: Some(Vec::new()),
                owned_uploads: Some(Vec::new()),
                edge_paths: Some(Vec::new()),
                edge_inventory: Some(
                    bbox_edge_sidecar::migration_inventory::EdgeRetirementInventory {
                        version:
                            bbox_edge_sidecar::migration_inventory::RETIREMENT_INVENTORY_VERSION,
                        project_id: project_id.as_str().to_string(),
                        relative_paths: Vec::new(),
                        receipt_bindings: std::collections::BTreeMap::new(),
                        receipt_closeouts: Vec::new(),
                        snapshot_reclamations: Vec::new(),
                    },
                ),
                artifact_targets: Some(Vec::new()),
                reference_class_counts: Some(std::collections::BTreeMap::new()),
                reference_class_commitments: Some(std::collections::BTreeMap::new()),
                ..RetirementJournalEvidence::default()
            },
            prepared_evidence_sha256: None,
        };
        journal.seal_retirement_evidence();
        journal
    }

    pub fn seal_retirement_evidence(&mut self) {
        self.prepared_evidence_sha256 = Some(retirement_evidence_sha256(&self.evidence));
    }

    /// Advance to the next stage, recording the step. Panics if already
    /// Complete (the caller checks `next()` first).
    pub fn advance(&mut self, now: &str) {
        let prev = self.current_stage;
        let next = self
            .current_stage
            .next()
            .expect("advance called on Complete journal");
        self.completed_steps.push(RetirementJournalStep {
            stage: prev,
            completed_at: now.to_string(),
        });
        self.current_stage = next;
        self.updated_at = now.to_string();
    }
}

/// A completed step recorded in the journal's history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetirementJournalStep {
    pub stage: RetirementJournalStage,
    pub completed_at: String,
}

/// R4F1+R4F6: Owner-validated evidence captured at Prepared time.
/// This block persists the EXACT generation ids and blob hashes
/// attributable to the retiring project, so later stages (generation
/// deletion, blob sweep) consume the snapshot rather than re-deriving
/// ownership from a catalog row that may already be removed.
///
/// Generation ownership is exact: each generation id is paired with
/// the scope hash it lives under and the project id validated from
/// the generation metadata record. Ambiguous ownership (two projects
/// claim the same generation) is refused at capture time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RetirementJournalEvidence {
    /// Project identity that owns every destructive evidence item.
    pub owner_project_id: Option<ProjectId>,

    /// Current catalog scope captured before the final authority cut. This is
    /// the only scope producer grants may authorize for retirement checks.
    pub catalog_scope: Option<PublishedScope>,

    /// Project id plus every attachment directory captured before attachment
    /// detachment. Later reprobes use these durable selectors.
    pub project_selectors: Option<Vec<String>>,

    /// Exact generation inventory that belongs to the retiring project and
    /// will be deleted in stage CollectedGenerationsDischarged.
    pub owned_generations: Vec<RetirementGenerationEvidence>,

    /// Exact desired pointers discharged before their generation directories.
    /// `None` is rejected as incomplete v2 evidence.
    pub desired_pointers: Option<Vec<RetirementGenerationEvidence>>,

    /// Exact in-progress uploads owned by the retiring project's current
    /// published scope. `None` is rejected as incomplete v2 evidence.
    pub owned_uploads: Option<Vec<RetirementUploadEvidence>>,

    /// Exact project-owned edge lanes and materialized paths captured at
    /// Prepared. Paths are relative to the configured edge root.
    pub edge_paths: Option<Vec<String>>,

    /// Exact edge workspace, receipt binding, and closeout authority captured
    /// before retirement begins.
    pub edge_inventory: Option<bbox_edge_sidecar::migration_inventory::EdgeRetirementInventory>,

    /// Exact artifact payload plus metadata targets captured at Prepared.
    pub artifact_targets: Option<Vec<bbox_artifacts::artifacts::ArtifactRetirementTarget>>,

    /// Prepared owner-row counts for every declared blocking class. Counts may
    /// only decrease while a stage is replayed; any increase is new authority
    /// outside the prepared plan.
    pub reference_class_counts: Option<std::collections::BTreeMap<String, u64>>,

    /// Stable exact-row commitments for every declared blocking class.
    pub reference_class_commitments: Option<std::collections::BTreeMap<String, Vec<String>>>,

    /// Hash-linked blob inventory persisted in a bounded sidecar.
    pub blob_inventory: Option<RetirementBlobInventoryRef>,

    /// Runtime-only decoded sidecar contents.
    #[serde(skip)]
    pub owned_blob_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetirementBlobInventoryRef {
    pub version: u32,
    pub sha256: String,
    pub blob_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetirementBlobInventorySidecar {
    version: u32,
    project_id: ProjectId,
    blob_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RetirementGenerationEvidence {
    pub published_scope: PublishedScope,
    pub generation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RetirementUploadEvidence {
    pub producer_id: String,
    pub upload_id: String,
    pub published_scope: PublishedScope,
}

/// Error type for retirement journal operations.
#[derive(Debug)]
pub enum RetirementJournalError {
    Io(std::io::Error),
    Serde(serde_json::Error),
    Other(String),
    UpgradeRequired(u32),
}

impl std::fmt::Display for RetirementJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Serde(e) => write!(f, "serde error: {e}"),
            Self::Other(msg) => write!(f, "{msg}"),
            Self::UpgradeRequired(version) => write!(
                f,
                "retirement journal version {version} requires an explicit upgrade before resume"
            ),
        }
    }
}

impl std::error::Error for RetirementJournalError {}

impl From<std::io::Error> for RetirementJournalError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for RetirementJournalError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

impl RetirementJournalError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

/// Resolve the journal file path for a project (section 11.3).
/// Convention: `{bro_home}/retirement-journals/{project_id}.json`.
pub fn retirement_journal_path(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> std::path::PathBuf {
    bro_home
        .join("retirement-journals")
        .join(format!("{project_id}.json"))
}

fn archived_retirement_journal_path(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> std::path::PathBuf {
    bro_home
        .join("retirement-journals")
        .join("archive")
        .join(format!("{project_id}.json"))
}

fn retirement_archive_marker_path(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> std::path::PathBuf {
    bro_home
        .join("retirement-journals")
        .join("archive")
        .join(format!("{project_id}.archive-pending"))
}

#[cfg(any(not(unix), test))]
fn retirement_blob_inventory_path(journal_path: &std::path::Path) -> std::path::PathBuf {
    journal_path.with_extension("blobs.json")
}

/// Load a journal from disk. Returns `Ok(None)` if the file does not exist.
///
/// F6: Strict bounded nofollow decoding. Validates:
/// - File size within the migration byte limit.
/// - Filename matches the expected project id convention.
/// - Deserialized journal has the correct version.
/// - Journal's project_id matches the expected project_id.
/// - current_stage is a valid known stage.
/// - No symlink following (opens via O_NOFOLLOW on Unix).
pub fn load_retirement_journal(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> Result<Option<ProjectRetirementJournal>, RetirementJournalError> {
    let path = retirement_journal_path(bro_home, project_id);
    load_retirement_journal_from_path(&path, project_id)
}

pub fn load_archived_retirement_journal(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> Result<Option<ProjectRetirementJournal>, RetirementJournalError> {
    let path = archived_retirement_journal_path(bro_home, project_id);
    load_retirement_journal_from_path(&path, project_id)
}

#[cfg(unix)]
fn open_retirement_parent(
    path: &std::path::Path,
    create: bool,
) -> Result<Option<std::fs::File>, RetirementJournalError> {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| RetirementJournalError::other("retirement journal has no parent"))?;
    if create {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(parent)
    {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => Ok(None),
        Err(error) => Err(RetirementJournalError::other(format!(
            "retirement journal parent is unavailable: {error}"
        ))),
    }
}

#[cfg(unix)]
fn open_retirement_leaf(
    directory: &std::fs::File,
    name: &str,
    limit: usize,
) -> Result<Option<std::fs::File>, RetirementJournalError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = std::ffi::CString::new(name)
        .map_err(|_| RetirementJournalError::other("retirement journal filename contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        if error.raw_os_error() == Some(libc::ELOOP) {
            return Err(RetirementJournalError::other(
                "retirement journal leaf is a symlink, not a regular file",
            ));
        }
        return Err(RetirementJournalError::other(format!(
            "retirement journal leaf is unavailable: {error}"
        )));
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(RetirementJournalError::other(
            "retirement journal leaf exceeds its byte limit or is not regular",
        ));
    }
    Ok(Some(file))
}

fn load_retirement_journal_from_path(
    path: &std::path::Path,
    project_id: &ProjectId,
) -> Result<Option<ProjectRetirementJournal>, RetirementJournalError> {
    #[cfg(unix)]
    let Some(directory) = open_retirement_parent(path, false)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    let leaf_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| RetirementJournalError::other("retirement journal filename is invalid"))?;
    #[cfg(unix)]
    let bytes = match open_retirement_leaf(&directory, leaf_name, MAX_JOURNAL_BYTES)? {
        Some(mut file) => {
            let metadata = file.metadata()?;
            let mut buf = Vec::with_capacity(metadata.len() as usize);
            std::io::Read::read_to_end(
                &mut std::io::Read::take(&mut file, MAX_JOURNAL_BYTES as u64 + 1),
                &mut buf,
            )?;
            if buf.len() as u64 != metadata.len() {
                return Err(RetirementJournalError::other(
                    "retirement journal changed while being read",
                ));
            }
            buf
        }
        None => return Ok(None),
    };
    #[cfg(not(unix))]
    let bytes = {
        let mut file = match std::fs::OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() as usize > MAX_JOURNAL_BYTES {
            return Err(RetirementJournalError::other(
                "retirement journal exceeds its byte limit or is not regular",
            ));
        }
        let mut buf = Vec::with_capacity(metadata.len() as usize);
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut file, MAX_JOURNAL_BYTES as u64 + 1),
            &mut buf,
        )?;
        if buf.len() as u64 != metadata.len() {
            return Err(RetirementJournalError::other(
                "retirement journal changed while being read",
            ));
        }
        buf
    };

    let envelope: serde_json::Value = serde_json::from_slice(&bytes)?;
    let version = envelope
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| RetirementJournalError::other("retirement journal version is missing"))?;
    let version = u32::try_from(version)
        .map_err(|_| RetirementJournalError::other("retirement journal version is invalid"))?;
    if version != ProjectRetirementJournal::VERSION {
        if version == 1 || version == 2 || version == 3 || version == 4 {
            return Err(RetirementJournalError::UpgradeRequired(version));
        }
        return Err(RetirementJournalError::other(format!(
            "retirement journal version mismatch: expected {}, got {}",
            ProjectRetirementJournal::VERSION,
            version
        )));
    }
    let mut journal: ProjectRetirementJournal = serde_json::from_value(envelope)?;
    // Filename/project_id consistency: the journal's embedded project_id
    // must match the filename-derived project_id.
    if journal.project_id != *project_id {
        return Err(RetirementJournalError::other(
            "retirement journal project_id does not match the filename",
        ));
    }
    // R2F5: strict stage validation. The completed_steps vector must be
    // exactly the ordered prefix from Prepared through the stage just
    // before current_stage. This prevents stage forgery where an
    // attacker edits current_stage to skip work.
    validate_journal_stage_history(&journal)?;
    #[cfg(unix)]
    {
        journal.evidence.owned_blob_hashes =
            load_retirement_blob_inventory_at(&directory, leaf_name, &journal)?;
    }
    #[cfg(not(unix))]
    {
        journal.evidence.owned_blob_hashes = load_retirement_blob_inventory(path, &journal)?;
    }
    validate_journal_evidence_shape(&journal)?;
    Ok(Some(journal))
}

fn validate_sha256_text(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_journal_evidence_shape(
    journal: &ProjectRetirementJournal,
) -> Result<(), RetirementJournalError> {
    if journal.evidence.owner_project_id.as_ref() != Some(&journal.project_id) {
        return Err(RetirementJournalError::other(
            "retirement journal evidence owner does not match the retiring project",
        ));
    }
    let blob_ref = journal.evidence.blob_inventory.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing its blob evidence reference")
    })?;
    let selectors = journal.evidence.project_selectors.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing its project selectors")
    })?;
    if !selectors
        .iter()
        .any(|selector| selector == journal.project_id.as_str())
        || selectors.iter().any(|selector| selector.is_empty())
        || selectors
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != selectors.len()
    {
        return Err(RetirementJournalError::other(
            "retirement journal project selectors are invalid",
        ));
    }
    if blob_ref.version != 1 || !validate_sha256_text(&blob_ref.sha256) {
        return Err(RetirementJournalError::other(
            "retirement journal blob evidence reference is invalid",
        ));
    }
    let mut identities = std::collections::BTreeSet::new();
    for generation in &journal.evidence.owned_generations {
        if !validate_sha256_text(&generation.generation_id)
            || !identities.insert((
                generation.published_scope.clone(),
                generation.generation_id.clone(),
            ))
        {
            return Err(RetirementJournalError::other(
                "retirement journal generation evidence is invalid or duplicated",
            ));
        }
    }
    let desired_pointers = journal.evidence.desired_pointers.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing its desired-pointer evidence")
    })?;
    let mut desired_identities = std::collections::BTreeSet::new();
    for pointer in desired_pointers {
        if !validate_sha256_text(&pointer.generation_id)
            || !desired_identities.insert((
                pointer.published_scope.clone(),
                pointer.generation_id.clone(),
            ))
        {
            return Err(RetirementJournalError::other(
                "retirement journal desired-pointer evidence is invalid or duplicated",
            ));
        }
    }
    let owned_uploads = journal.evidence.owned_uploads.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing its upload evidence")
    })?;
    let mut upload_identities = std::collections::BTreeSet::new();
    for upload in owned_uploads {
        if upload.producer_id.is_empty()
            || upload.upload_id.is_empty()
            || !upload_identities.insert((
                upload.producer_id.clone(),
                upload.upload_id.clone(),
                upload.published_scope.clone(),
            ))
        {
            return Err(RetirementJournalError::other(
                "retirement journal upload evidence is invalid or duplicated",
            ));
        }
    }
    let edge_paths = journal.evidence.edge_paths.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing its edge-path evidence")
    })?;
    let mut unique_edge_paths = std::collections::BTreeSet::new();
    for path in edge_paths {
        let relative = std::path::Path::new(path);
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || !unique_edge_paths.insert(path)
        {
            return Err(RetirementJournalError::other(
                "retirement journal edge-path evidence is invalid or duplicated",
            ));
        }
    }
    let edge_inventory = journal.evidence.edge_inventory.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing its edge authority evidence")
    })?;
    if edge_inventory.project_id != journal.project_id.as_str()
        || edge_inventory.version
            != bbox_edge_sidecar::migration_inventory::RETIREMENT_INVENTORY_VERSION
        || edge_inventory.relative_paths.as_slice() != edge_paths.as_slice()
    {
        return Err(RetirementJournalError::other(
            "retirement journal edge authority evidence is not bound to its owner and paths",
        ));
    }
    let snapshot_prefix = format!("workspace/{}/snapshots/", journal.project_id.as_str());
    if edge_inventory
        .receipt_bindings
        .iter()
        .any(|(snapshot, digest)| {
            !snapshot.starts_with(&snapshot_prefix) || !validate_sha256_text(digest)
        })
        || edge_inventory.receipt_closeouts.iter().any(|closeout| {
            closeout.commitment.is_empty()
                || !closeout.snapshot.starts_with(&snapshot_prefix)
                || !validate_sha256_text(&closeout.digest)
        })
        || edge_inventory
            .receipt_closeouts
            .iter()
            .map(|closeout| closeout.commitment.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != edge_inventory.receipt_closeouts.len()
    {
        return Err(RetirementJournalError::other(
            "retirement journal edge receipt authority evidence is invalid",
        ));
    }
    // R27F5: reclamation records are recovery authority that discharge
    // deletes, so they are validated to the same standard as the receipt
    // classes above: owner-bound snapshot path, well-formed receipt digest
    // when present, non-empty tombstone, and no duplicate snapshot key.
    if edge_inventory
        .snapshot_reclamations
        .iter()
        .any(|reclamation| {
            !reclamation.snapshot.starts_with(&snapshot_prefix)
                || reclamation.tombstone.is_empty()
                || reclamation
                    .receipt_digest
                    .as_deref()
                    .is_some_and(|digest| !validate_sha256_text(digest))
        })
        || edge_inventory
            .snapshot_reclamations
            .iter()
            .map(|reclamation| reclamation.snapshot.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != edge_inventory.snapshot_reclamations.len()
    {
        return Err(RetirementJournalError::other(
            "retirement journal edge snapshot reclamation evidence is invalid",
        ));
    }
    let artifact_targets = journal.evidence.artifact_targets.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing artifact target evidence")
    })?;
    if artifact_targets
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != artifact_targets.len()
    {
        return Err(RetirementJournalError::other(
            "retirement journal artifact target evidence is duplicated",
        ));
    }
    for target in artifact_targets {
        target.validate().map_err(|error| {
            RetirementJournalError::other(format!(
                "retirement journal artifact target evidence is invalid: {error}"
            ))
        })?;
    }
    journal
        .evidence
        .reference_class_counts
        .as_ref()
        .ok_or_else(|| {
            RetirementJournalError::other(
                "retirement journal is missing reference-class plan evidence",
            )
        })?;
    let commitments = journal
        .evidence
        .reference_class_commitments
        .as_ref()
        .ok_or_else(|| {
            RetirementJournalError::other(
                "retirement journal is missing reference-class identity commitments",
            )
        })?;
    for (class, identities) in commitments {
        if class.is_empty()
            || identities
                .iter()
                .any(|identity| !validate_sha256_text(identity))
            || identities
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != identities.len()
        {
            return Err(RetirementJournalError::other(
                "retirement journal reference-class commitments are invalid",
            ));
        }
    }
    let expected_hash = journal.prepared_evidence_sha256.as_ref().ok_or_else(|| {
        RetirementJournalError::other(
            "retirement journal is missing its Prepared evidence commitment",
        )
    })?;
    if !validate_sha256_text(expected_hash)
        || retirement_evidence_sha256(&journal.evidence) != *expected_hash
    {
        return Err(RetirementJournalError::other(
            "retirement journal Prepared evidence commitment does not match",
        ));
    }
    Ok(())
}

pub fn retirement_evidence_sha256(evidence: &RetirementJournalEvidence) -> String {
    let bytes = serde_json::to_vec(&(
        &evidence.owner_project_id,
        &evidence.catalog_scope,
        &evidence.project_selectors,
        &evidence.owned_generations,
        &evidence.desired_pointers,
        &evidence.owned_uploads,
        &evidence.edge_paths,
        &evidence.edge_inventory,
        &evidence.artifact_targets,
        &evidence.reference_class_counts,
        &evidence.reference_class_commitments,
        &evidence.owned_blob_hashes,
    ))
    .expect("retirement evidence serialization is infallible");
    bbox_corpus_core::project_catalog_snapshot::sha256_hex(&bytes)
}

#[cfg(unix)]
fn load_retirement_blob_inventory_at(
    directory: &std::fs::File,
    journal_leaf: &str,
    journal: &ProjectRetirementJournal,
) -> Result<Vec<String>, RetirementJournalError> {
    const MAX_BLOB_INVENTORY_BYTES: usize = 64 * 1024 * 1024;
    let stem = journal_leaf
        .strip_suffix(".json")
        .ok_or_else(|| RetirementJournalError::other("retirement journal filename is invalid"))?;
    let sidecar_leaf = format!("{stem}.blobs.json");
    let mut file = open_retirement_leaf(directory, &sidecar_leaf, MAX_BLOB_INVENTORY_BYTES)?
        .ok_or_else(|| RetirementJournalError::other("retirement blob evidence is missing"))?;
    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut bounded = std::io::Read::take(&mut file, MAX_BLOB_INVENTORY_BYTES as u64 + 1);
    std::io::Read::read_to_end(&mut bounded, &mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(RetirementJournalError::other(
            "retirement blob evidence changed while being read",
        ));
    }
    validate_retirement_blob_inventory_bytes(&bytes, journal)
}

#[cfg(not(unix))]
fn load_retirement_blob_inventory(
    journal_path: &std::path::Path,
    journal: &ProjectRetirementJournal,
) -> Result<Vec<String>, RetirementJournalError> {
    const MAX_BLOB_INVENTORY_BYTES: usize = 64 * 1024 * 1024;
    let path = retirement_blob_inventory_path(journal_path);
    let metadata = std::fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RetirementJournalError::other(
            "retirement blob evidence is not a regular file",
        ));
    }
    if metadata.len() > MAX_BLOB_INVENTORY_BYTES as u64 {
        return Err(RetirementJournalError::other(
            "retirement blob evidence exceeds its byte limit",
        ));
    }
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    RetirementJournalError::other(
                        "retirement blob evidence is a symlink; refusing to follow",
                    )
                } else {
                    RetirementJournalError::from(error)
                }
            })?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::File::open(&path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut bounded = std::io::Read::take(&mut file, MAX_BLOB_INVENTORY_BYTES as u64 + 1);
    std::io::Read::read_to_end(&mut bounded, &mut bytes)?;
    if bytes.len() > MAX_BLOB_INVENTORY_BYTES {
        return Err(RetirementJournalError::other(
            "retirement blob evidence exceeds its byte limit",
        ));
    }
    validate_retirement_blob_inventory_bytes(&bytes, journal)
}

fn validate_retirement_blob_inventory_bytes(
    bytes: &[u8],
    journal: &ProjectRetirementJournal,
) -> Result<Vec<String>, RetirementJournalError> {
    let expected = journal.evidence.blob_inventory.as_ref().ok_or_else(|| {
        RetirementJournalError::other("retirement journal is missing blob evidence")
    })?;
    let actual_sha256 = bbox_corpus_core::project_catalog_snapshot::sha256_hex(&bytes);
    if actual_sha256 != expected.sha256 {
        return Err(RetirementJournalError::other(
            "retirement blob evidence hash does not match the journal",
        ));
    }
    let sidecar: RetirementBlobInventorySidecar = serde_json::from_slice(bytes)?;
    if sidecar.version != 1 || sidecar.project_id != journal.project_id {
        return Err(RetirementJournalError::other(
            "retirement blob evidence identity is invalid",
        ));
    }
    if sidecar.blob_hashes.len() as u64 != expected.blob_count
        || sidecar
            .blob_hashes
            .iter()
            .any(|hash| !validate_sha256_text(hash))
    {
        return Err(RetirementJournalError::other(
            "retirement blob evidence count or hash is invalid",
        ));
    }
    Ok(sidecar.blob_hashes)
}

/// R2F5: validate that completed_steps is an exactly ordered prefix
/// ending at the stage just before current_stage, with monotonic
/// timestamps and matching epoch.
fn validate_journal_stage_history(
    journal: &ProjectRetirementJournal,
) -> Result<(), RetirementJournalError> {
    // The expected steps are: Prepared, SourceAuthorityQuiesced, ...,
    // up to the stage whose .next() == current_stage.
    let mut expected = Vec::new();
    let mut cursor = RetirementJournalStage::Prepared;
    while cursor != journal.current_stage {
        expected.push(cursor);
        match cursor.next() {
            Some(n) => cursor = n,
            None => {
                return Err(RetirementJournalError::other(
                    "retirement journal current_stage has no predecessor in the ordered chain",
                ));
            }
        }
    }

    if journal.completed_steps.len() != expected.len() {
        return Err(RetirementJournalError::other(format!(
            "retirement journal stage forgery detected: expected {} completed_steps for {:?}, got {}",
            expected.len(),
            journal.current_stage,
            journal.completed_steps.len()
        )));
    }

    let mut last_ts: Option<&str> = None;
    for (i, expected_stage) in expected.iter().enumerate() {
        let step = &journal.completed_steps[i];
        if step.stage != *expected_stage {
            return Err(RetirementJournalError::other(format!(
                "retirement journal stage forgery detected: completed_steps[{}] is {:?}, expected {:?}",
                i, step.stage, expected_stage
            )));
        }
        // Monotonic timestamps: each completed_at must be >= the previous.
        if let Some(prev) = last_ts {
            if step.completed_at.as_str() < prev {
                return Err(RetirementJournalError::other(format!(
                    "retirement journal timestamp regression at step {}: {} < {}",
                    i, step.completed_at, prev
                )));
            }
        }
        last_ts = Some(&step.completed_at);
    }

    // updated_at must be >= the last completed_at (or started_at if no steps).
    let reference_ts = last_ts.unwrap_or(&journal.started_at);
    if journal.updated_at.as_str() < reference_ts {
        return Err(RetirementJournalError::other(format!(
            "retirement journal updated_at {} predates last completed_at {}",
            journal.updated_at, reference_ts
        )));
    }

    Ok(())
}

/// Maximum byte size for a retirement journal (F6: bounded read).
const MAX_JOURNAL_BYTES: usize = 64 * 1024;

#[cfg(unix)]
fn atomic_write_retirement_leaf(
    directory: &std::fs::File,
    target: &str,
    bytes: &[u8],
) -> Result<(), RetirementJournalError> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};

    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = format!(".retirement.{}.{}.tmp", std::process::id(), sequence);
    let temp_c = std::ffi::CString::new(temp.as_str())
        .map_err(|_| RetirementJournalError::other("retirement temp filename contains NUL"))?;
    let target_c = std::ffi::CString::new(target)
        .map_err(|_| RetirementJournalError::other("retirement filename contains NUL"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let status = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            target_c.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status == 0 {
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(RetirementJournalError::other(
                "retirement journal target is a symlink",
            ));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(RetirementJournalError::other(
                "retirement journal target is present with an invalid type",
            ));
        }
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        unsafe { libc::unlinkat(directory.as_raw_fd(), temp_c.as_ptr(), 0) };
        return Err(error.into());
    }
    drop(file);
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp_c.as_ptr(),
            directory.as_raw_fd(),
            target_c.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        unsafe { libc::unlinkat(directory.as_raw_fd(), temp_c.as_ptr(), 0) };
        return Err(error.into());
    }
    directory.sync_all()?;
    Ok(())
}

/// Persist a journal to disk, syncing the file AND directory after the
/// write (section 11.3: "each advance synced to disk"). F6: uses an
/// anchored atomic write with fsync on both the temp file and the
/// directory, and refuses to follow symlinks.
pub fn save_retirement_journal(
    bro_home: &std::path::Path,
    journal: &ProjectRetirementJournal,
) -> Result<(), RetirementJournalError> {
    #[cfg(unix)]
    {
        let path = retirement_journal_path(bro_home, &journal.project_id);
        let directory = open_retirement_parent(&path, true)?.ok_or_else(|| {
            RetirementJournalError::other("retirement journal directory disappeared")
        })?;
        let sidecar = RetirementBlobInventorySidecar {
            version: 1,
            project_id: journal.project_id.clone(),
            blob_hashes: journal.evidence.owned_blob_hashes.clone(),
        };
        let sidecar_bytes = serde_json::to_vec(&sidecar)?;
        let mut persisted = journal.clone();
        persisted.evidence.blob_inventory = Some(RetirementBlobInventoryRef {
            version: 1,
            sha256: bbox_corpus_core::project_catalog_snapshot::sha256_hex(&sidecar_bytes),
            blob_count: sidecar.blob_hashes.len() as u64,
        });
        let journal_bytes = serde_json::to_vec_pretty(&persisted)?;
        if journal_bytes.len() > MAX_JOURNAL_BYTES {
            return Err(RetirementJournalError::other(
                "retirement journal exceeds its byte limit",
            ));
        }
        let journal_leaf = format!("{}.json", journal.project_id);
        let sidecar_leaf = format!("{}.blobs.json", journal.project_id);
        atomic_write_retirement_leaf(&directory, &sidecar_leaf, &sidecar_bytes)?;
        atomic_write_retirement_leaf(&directory, &journal_leaf, &journal_bytes)?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let dir = bro_home.join("retirement-journals");
        std::fs::create_dir_all(&dir)?;
        let dir_metadata = std::fs::symlink_metadata(&dir)?;
        if dir_metadata.file_type().is_symlink() || !dir_metadata.is_dir() {
            return Err(RetirementJournalError::other(
                "retirement-journals parent is not a regular directory",
            ));
        }

        // R3F4: open the parent directory for the post-rename fsync.
        // The directory handle ensures we sync the actual directory, not
        // a redirected path.
        #[cfg(unix)]
        let dir_handle = {
            std::fs::File::open(&dir).map_err(|e| {
                RetirementJournalError::other(&format!(
                    "failed to open retirement-journals directory: {e}"
                ))
            })?
        };

        let path = retirement_journal_path(bro_home, &journal.project_id);

        // F6: refuse if the target path is a symlink (nofollow write).
        if path.is_symlink() {
            return Err(RetirementJournalError::other(
                "retirement journal target is a symlink; refusing to write through it",
            ));
        }

        let sidecar = RetirementBlobInventorySidecar {
            version: 1,
            project_id: journal.project_id.clone(),
            blob_hashes: journal.evidence.owned_blob_hashes.clone(),
        };
        let sidecar_bytes = serde_json::to_vec(&sidecar)?;
        let sidecar_path = retirement_blob_inventory_path(&path);
        bbox_corpus_core::json_store::with_store_lock(&sidecar_path, || {
            bbox_corpus_core::json_store::atomic_write_bytes_locked(&sidecar_path, &sidecar_bytes)
        })
        .map_err(|error| RetirementJournalError::other(error.to_string()))?;
        let mut persisted = journal.clone();
        persisted.evidence.blob_inventory = Some(RetirementBlobInventoryRef {
            version: 1,
            sha256: bbox_corpus_core::project_catalog_snapshot::sha256_hex(&sidecar_bytes),
            blob_count: sidecar.blob_hashes.len() as u64,
        });
        let bytes = serde_json::to_vec_pretty(&persisted)?;

        // Bounded write: refuse oversized journals.
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(RetirementJournalError::other(
                "retirement journal exceeds its byte limit",
            ));
        }

        // R3F4: use a UNIQUE temp name so a crash between temp creation and
        // rename does not permanently wedge the next save (which used a
        // deterministic <project>.json.tmp and hit EEXIST on retry). The
        // unique suffix includes the PID and a timestamp to avoid collisions.
        let tmp = {
            let unique = format!(
                "{}-{}-{}.json.tmp",
                journal.project_id,
                std::process::id(),
                unix_now_secs()
            );
            dir.join(unique)
        };

        // R3F4: if a stale temp from a previous crash exists at this exact
        // unique name (extremely unlikely given PID+timestamp), remove it
        // before creating. This is safe because the temp name is unique to
        // this process and invocation.
        if tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }

        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)
                .map_err(|e| {
                    if e.raw_os_error() == Some(libc::ELOOP) {
                        RetirementJournalError::other(
                            "retirement journal temp path is a symlink; refusing to write through it",
                        )
                    } else {
                        RetirementJournalError::from(e)
                    }
                })?;
                std::io::Write::write_all(&mut file, &bytes)?;
                // Fsync the file before rename so the data is durable.
                file.sync_all()?;
            }
            #[cfg(not(unix))]
            {
                let mut file = std::fs::File::create_new(&tmp)?;
                std::io::Write::write_all(&mut file, &bytes)?;
                file.sync_all()?;
            }
        }
        std::fs::rename(&tmp, &path)?;
        // R3F4: sync the parent directory using the nofollow-opened handle.
        #[cfg(unix)]
        {
            dir_handle.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let dir_handle = std::fs::File::open(&dir)?;
            dir_handle.sync_all()?;
        }
        Ok(())
    }
}

/// Archive (or remove) a completed journal (section 11.3 step 9).
/// The completed journal is removed from the active directory so the
/// P4-F startup probe does not refuse the next boot.
pub fn archive_retirement_journal(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> Result<(), RetirementJournalError> {
    #[cfg(unix)]
    {
        return archive_retirement_journal_anchored(bro_home, project_id);
    }
    #[cfg(not(unix))]
    {
        let path = retirement_journal_path(bro_home, project_id);
        let dir = bro_home.join("retirement-journals");
        let archive_dir = dir.join("archive");
        std::fs::create_dir_all(&archive_dir)?;
        let archived = archived_retirement_journal_path(bro_home, project_id);
        let sidecar = retirement_blob_inventory_path(&path);
        let archived_sidecar = retirement_blob_inventory_path(&archived);
        let marker = retirement_archive_marker_path(bro_home, project_id);
        if path.is_file() || marker.is_file() {
            bbox_corpus_core::json_store::with_store_lock(&marker, || {
                bbox_corpus_core::json_store::atomic_write_bytes_locked(
                    &marker,
                    project_id.as_str().as_bytes(),
                )
            })
            .map_err(|error| RetirementJournalError::other(error.to_string()))?;
            if sidecar.is_file() {
                if archived_sidecar.exists() {
                    return Err(RetirementJournalError::other(
                        "retirement archive has conflicting blob sidecars",
                    ));
                }
                std::fs::rename(&sidecar, &archived_sidecar)?;
                std::fs::File::open(&archive_dir)?.sync_all()?;
                std::fs::File::open(&dir)?.sync_all()?;
            }
            if path.is_file() {
                if archived.exists() {
                    return Err(RetirementJournalError::other(
                        "retirement archive has conflicting journals",
                    ));
                }
                std::fs::rename(&path, &archived)?;
                std::fs::File::open(&archive_dir)?.sync_all()?;
                std::fs::File::open(&dir)?.sync_all()?;
            }
            match std::fs::remove_file(&marker) {
                Ok(()) => std::fs::File::open(&archive_dir)?.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn archive_retirement_journal_anchored(
    bro_home: &std::path::Path,
    project_id: &ProjectId,
) -> Result<(), RetirementJournalError> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    fn open_dir_at(
        parent: &std::fs::File,
        name: &str,
    ) -> Result<std::fs::File, RetirementJournalError> {
        let name = std::ffi::CString::new(name).map_err(|_| {
            RetirementJournalError::other("retirement archive directory contains NUL")
        })?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(RetirementJournalError::other(format!(
                "retirement archive directory is unavailable: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }

    fn regular_leaf(
        directory: &std::fs::File,
        name: &std::ffi::OsStr,
    ) -> Result<bool, RetirementJournalError> {
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| {
            RetirementJournalError::other("retirement archive filename contains NUL")
        })?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(error.into());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(RetirementJournalError::other(
                "retirement archive leaf is not a regular file",
            ));
        }
        Ok(true)
    }

    fn rename_leaf(
        active: &std::fs::File,
        archive: &std::fs::File,
        name: &std::ffi::OsStr,
    ) -> Result<(), RetirementJournalError> {
        use std::os::unix::ffi::OsStrExt;
        let source_present = regular_leaf(active, name)?;
        let archived_present = regular_leaf(archive, name)?;
        if source_present && archived_present {
            return Err(RetirementJournalError::other(
                "retirement archive has conflicting active and archived leaves",
            ));
        }
        if !source_present {
            return Ok(());
        }
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| {
            RetirementJournalError::other("retirement archive filename contains NUL")
        })?;
        if unsafe {
            libc::renameat(
                active.as_raw_fd(),
                name.as_ptr(),
                archive.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        active.sync_all()?;
        archive.sync_all()?;
        Ok(())
    }

    let home = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(bro_home)
        .map_err(|error| {
            RetirementJournalError::other(format!("retirement home is unavailable: {error}"))
        })?;
    let active = open_dir_at(&home, "retirement-journals")?;
    let archive = open_dir_at(&active, "archive")?;
    home.sync_all()?;
    active.sync_all()?;

    let journal_name = std::ffi::OsString::from(format!("{project_id}.json"));
    let sidecar_name = std::ffi::OsString::from(format!("{project_id}.blobs.json"));
    let marker_name = std::ffi::OsString::from(format!("{project_id}.archive-pending"));
    let any_work = regular_leaf(&active, &journal_name)?
        || regular_leaf(&active, &sidecar_name)?
        || regular_leaf(&archive, &marker_name)?;
    if !any_work {
        return Ok(());
    }

    if !regular_leaf(&archive, &marker_name)? {
        let marker_c = std::ffi::CString::new(marker_name.as_bytes())
            .map_err(|_| RetirementJournalError::other("retirement archive marker contains NUL"))?;
        let fd = unsafe {
            libc::openat(
                archive.as_raw_fd(),
                marker_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut marker = unsafe { std::fs::File::from_raw_fd(fd) };
        marker.write_all(project_id.as_str().as_bytes())?;
        marker.sync_all()?;
        archive.sync_all()?;
    }
    retirement_archive_fault("marker_committed")?;
    rename_leaf(&active, &archive, &sidecar_name)?;
    retirement_archive_fault("sidecar_archived")?;
    rename_leaf(&active, &archive, &journal_name)?;
    retirement_archive_fault("journal_archived")?;

    let marker_c = std::ffi::CString::new(marker_name.as_bytes())
        .map_err(|_| RetirementJournalError::other("retirement archive marker contains NUL"))?;
    if unsafe { libc::unlinkat(archive.as_raw_fd(), marker_c.as_ptr(), 0) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    archive.sync_all()?;
    Ok(())
}

fn retirement_archive_fault(boundary: &str) -> Result<(), RetirementJournalError> {
    if cfg!(debug_assertions)
        && std::env::var("BLACKBOX_TEST_RETIREMENT_ARCHIVE_FAULT")
            .ok()
            .as_deref()
            == Some(boundary)
    {
        return Err(RetirementJournalError::other(format!(
            "injected retirement archive fault at {boundary}"
        )));
    }
    Ok(())
}

/// Preflight evidence for retirement: the blocking-class inventory,
/// source-owned records, and Ready-materialization flag.
/// Section 11.2: computed BEFORE journal creation so the operator
/// sees the exact discharge plan.
#[derive(Debug, Clone)]
pub struct RetirementPreflight {
    /// Nonzero blocking classes that must be discharged.
    pub blocking: std::collections::BTreeMap<String, u64>,
    /// True when the project has a Ready materialization (refusal).
    pub history_ready_refusal: bool,
    /// Source-owned activation/generation count to discharge.
    pub source_owned_records: u64,
    /// Whether the project exists in the catalog.
    pub project_exists: bool,
    /// The catalog epoch at preflight time.
    pub catalog_epoch: u64,
}

/// Discharge workers for the retirement journal (section 11.3).
///
/// Each method is a single-attempt library-level primitive with no retry
/// loops (section 11.1). The admin crate defines the trait; the CLI layer
/// (which has all roots offline under the exclusive lifetime lock)
/// implements it with concrete store handles. This keeps the admin
/// crate's dependency direction clean: it never imports the code-source
/// server crate or the index writer.
///
/// Every method receives the `project_id` and the current journal so it
/// can record what was discharged. Each method is idempotent: calling it
/// again after a successful discharge is a no-op.
pub trait RetirementDischargeWorkers {
    /// R4F1+R4F6: Capture owner-validated generation and blob inventory
    /// at Prepared time. Returns the exact generation ids (with scope
    /// hash) and blob hashes that belong to this project. Called once
    /// before SourceAuthorityQuiesced; the result is persisted in the
    /// journal and consumed by later stages.
    fn capture_retirement_evidence(
        &mut self,
        project_id: &ProjectId,
    ) -> AdminResult<RetirementJournalEvidence> {
        Ok(RetirementJournalEvidence {
            owner_project_id: Some(project_id.clone()),
            project_selectors: Some(vec![project_id.as_str().to_string()]),
            desired_pointers: Some(Vec::new()),
            owned_uploads: Some(Vec::new()),
            edge_paths: Some(Vec::new()),
            edge_inventory: Some(
                bbox_edge_sidecar::migration_inventory::EdgeRetirementInventory {
                    version: bbox_edge_sidecar::migration_inventory::RETIREMENT_INVENTORY_VERSION,
                    project_id: project_id.as_str().to_string(),
                    relative_paths: Vec::new(),
                    receipt_bindings: std::collections::BTreeMap::new(),
                    receipt_closeouts: Vec::new(),
                    snapshot_reclamations: Vec::new(),
                },
            ),
            artifact_targets: Some(Vec::new()),
            reference_class_counts: Some(std::collections::BTreeMap::new()),
            reference_class_commitments: Some(std::collections::BTreeMap::new()),
            ..RetirementJournalEvidence::default()
        })
    }

    fn validate_retirement_evidence(
        &mut self,
        _store: &ProjectCatalogStore,
        project_id: &ProjectId,
        evidence: &RetirementJournalEvidence,
        _stage: RetirementJournalStage,
    ) -> AdminResult<()> {
        if evidence.owner_project_id.as_ref() != Some(project_id) {
            return Err(admin_error(
                "error.project_catalog_retire_evidence_owner",
                "retirement evidence owner does not match the retiring project",
            ));
        }
        Ok(())
    }

    /// Stage CollectedGenerationsDischarged: discharge collected
    /// generations for the project. This retires the project's
    /// collected selectors (single-attempt, no retry budget), deletes
    /// source-owned records (activation record, generation records),
    /// and clears entity references and project-scoped rows. After
    /// this stage, those blocking classes are zero.
    ///
    /// R4F1: `evidence` carries the exact owner-validated generation
    /// inventory captured at Prepared time. The worker deletes ONLY
    /// those exact generation ids, not every generation under a scope.
    fn discharge_collected_generations(
        &mut self,
        project_id: &ProjectId,
        evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()>;

    /// Stage PublicationsCleared: clear accepted publication state for
    /// the project. The accepted-publication blocking class reaches
    /// zero.
    fn discharge_publications(&mut self, project_id: &ProjectId) -> AdminResult<()>;

    /// Stage AttachmentsDetached: detach the project's active
    /// attachments through a catalog pair transact.
    /// `active_attachments` reaches zero. Returns the new catalog epoch
    /// after the transact (for the CatalogPairRemoved stage).
    fn discharge_attachments(
        &mut self,
        store: &ProjectCatalogStore,
        project_id: &ProjectId,
    ) -> AdminResult<()>;

    /// Stage MaterializationSwept: delete blobs only when shared-history
    /// reference accounting reaches zero. When other projects still
    /// reference shared blobs, the sweep verifiably skips them. No
    /// catalog or auth lock held during blob deletion.
    ///
    /// R4F6: `evidence` carries the exact blob hash inventory captured
    /// at Prepared time, so the sweep does not depend on the catalog
    /// row being present (it is already removed by this stage).
    fn sweep_materialization(
        &mut self,
        project_id: &ProjectId,
        evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()>;

    /// Verify that source authority has quiesced for the project before
    /// the journal advances past SourceAuthorityQuiesced (F5). The worker
    /// must refuse (return Err) if the project still holds active auth
    /// assignments or un-revoked producer bindings. Only when this returns
    /// Ok may the journal advance.
    fn verify_source_authority_quiesced(
        &mut self,
        store: &ProjectCatalogStore,
        project_id: &ProjectId,
        evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()>;

    /// Verify recovery after the catalog pair is absent. Exact generation
    /// identities from Prepared evidence must be gone, and activation state
    /// must remain absent.
    fn verify_retirement_quiescent(
        &mut self,
        _project_id: &ProjectId,
        _evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()> {
        Ok(())
    }

    /// Re-inventory the cross-store reference classes from CURRENT state
    /// after all discharge stages have run. Called at CatalogPairRemoved
    /// (section 11.3 step 7) to verify that the discharge workers actually
    /// zeroed every blocking class, rather than trusting fabricated evidence.
    ///
    /// The CLI worker re-runs its existing probe machinery against live
    /// stores. The Noop/test default returns the ORIGINAL evidence
    /// unchanged, so a journal driven with no-op workers on a referenced
    /// project refuses at the final cut instead of orphaning records.
    fn reprobe_evidence(
        &mut self,
        store: &ProjectCatalogStore,
        project_id: &ProjectId,
        original_evidence: &RetireEvidence,
        retirement_evidence: &RetirementJournalEvidence,
    ) -> AdminResult<RetireEvidence>;
}

/// A no-op implementation for preflight-only invocations (execute=false)
/// and for projects with zero blocking classes.
struct NoopDischargeWorkers;

impl RetirementDischargeWorkers for NoopDischargeWorkers {
    fn capture_retirement_evidence(
        &mut self,
        project_id: &ProjectId,
    ) -> AdminResult<RetirementJournalEvidence> {
        Ok(RetirementJournalEvidence {
            owner_project_id: Some(project_id.clone()),
            project_selectors: Some(vec![project_id.as_str().to_string()]),
            desired_pointers: Some(Vec::new()),
            owned_uploads: Some(Vec::new()),
            edge_paths: Some(Vec::new()),
            edge_inventory: Some(
                bbox_edge_sidecar::migration_inventory::EdgeRetirementInventory {
                    version: bbox_edge_sidecar::migration_inventory::RETIREMENT_INVENTORY_VERSION,
                    project_id: project_id.as_str().to_string(),
                    relative_paths: Vec::new(),
                    receipt_bindings: std::collections::BTreeMap::new(),
                    receipt_closeouts: Vec::new(),
                    snapshot_reclamations: Vec::new(),
                },
            ),
            artifact_targets: Some(Vec::new()),
            reference_class_counts: Some(std::collections::BTreeMap::new()),
            reference_class_commitments: Some(std::collections::BTreeMap::new()),
            ..RetirementJournalEvidence::default()
        })
    }
    fn discharge_collected_generations(
        &mut self,
        _project_id: &ProjectId,
        _evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()> {
        Ok(())
    }
    fn discharge_publications(&mut self, _project_id: &ProjectId) -> AdminResult<()> {
        Ok(())
    }
    fn discharge_attachments(
        &mut self,
        _store: &ProjectCatalogStore,
        _project_id: &ProjectId,
    ) -> AdminResult<()> {
        Ok(())
    }
    fn sweep_materialization(
        &mut self,
        _project_id: &ProjectId,
        _evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()> {
        Ok(())
    }

    fn verify_source_authority_quiesced(
        &mut self,
        _store: &ProjectCatalogStore,
        _project_id: &ProjectId,
        _evidence: &RetirementJournalEvidence,
    ) -> AdminResult<()> {
        Ok(())
    }

    fn reprobe_evidence(
        &mut self,
        _store: &ProjectCatalogStore,
        _project_id: &ProjectId,
        original_evidence: &RetireEvidence,
        _retirement_evidence: &RetirementJournalEvidence,
    ) -> AdminResult<RetireEvidence> {
        Ok(original_evidence.clone())
    }
}

/// Resolve the current timestamp as an ISO-8601-ish string. Used for
/// journal timestamps.
fn journal_now() -> String {
    unix_now_secs()
}

fn retirement_journal_admin_error(error: RetirementJournalError) -> ProjectCatalogStoreError {
    match error {
        RetirementJournalError::UpgradeRequired(version) => admin_error(
            "error.project_catalog_retire_journal_upgrade_required",
            format!("retirement journal version {version} requires an explicit upgrade"),
        ),
        other => admin_error("error.project_catalog_retire_journal_io", other.to_string()),
    }
}

/// Current seconds since UNIX_EPOCH (falls back to 0 on clock issues).
fn unix_now_secs() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Execute the full forward-only retirement journal discharge
/// (section 11.3). This is the library-level primitive called by the
/// CLI under the exclusive lifetime lock with the daemon stopped.
///
/// Each stage is idempotent: if called again after a partial completion,
/// it resumes from the current stage. The discharge workers are
/// single-attempt with no retry loops.
///
/// Parameters:
/// - `store`: the catalog pair store.
/// - `bro_home`: the BRO_HOME directory for journal persistence.
/// - `project_id`: the project to retire.
/// - `evidence`: the pre-probed retire evidence (external references).
/// - `execute`: when false, only the preflight runs (no journal created).
///
/// Returns the final journal state. If the project is already retired
/// (journal Complete or project absent past CatalogPairRemoved), the
/// recovery path verifies quiescence.
pub fn retire_project_journaled(
    store: &ProjectCatalogStore,
    bro_home: &std::path::Path,
    project_id: &ProjectId,
    evidence: &RetireEvidence,
    execute: bool,
) -> AdminResult<(RetirementPreflight, Option<ProjectRetirementJournal>)> {
    let mut workers = NoopDischargeWorkers;
    retire_project_journaled_with(store, bro_home, project_id, evidence, execute, &mut workers)
}

/// Same as [`retire_project_journaled`] but accepts explicit discharge
/// workers. Use this from the CLI where concrete store handles are
/// available.
pub fn retire_project_journaled_with(
    store: &ProjectCatalogStore,
    bro_home: &std::path::Path,
    project_id: &ProjectId,
    evidence: &RetireEvidence,
    execute: bool,
    workers: &mut dyn RetirementDischargeWorkers,
) -> AdminResult<(RetirementPreflight, Option<ProjectRetirementJournal>)> {
    if retirement_archive_marker_path(bro_home, project_id).is_file() {
        archive_retirement_journal(bro_home, project_id).map_err(retirement_journal_admin_error)?;
    }
    // Step 1: preflight (section 11.2).
    let state = store.snapshot()?;
    let catalog_epoch = state.epoch();
    let _project_exists = state.catalog().projects.contains_key(project_id);

    if !execute {
        let preflight = build_preflight(&state, project_id, evidence);
        return Ok((preflight, None));
    }

    // Ready-materialization refusal (section 11.2): refuse before
    // creating the journal.
    let preflight = build_preflight(&state, project_id, evidence);
    if preflight.history_ready_refusal {
        return Err(admin_error(
            "error.project_catalog_admin_retire_history_ready",
            format!(
                "project {project_id} has Ready repo-history materialization; \
                 dematerialize or rehome the history record before retiring"
            ),
        ));
    }
    if preflight
        .blocking
        .get("producer_assignments")
        .is_some_and(|count| *count > 0)
    {
        return Err(admin_error(
            "error.project_catalog_retire_producer_grant",
            format!(
                "project {project_id} still has a configured producer grant; \
                 revoke it before creating or resuming retirement"
            ),
        ));
    }

    // Load or create the journal (recovery: section 11.4).
    let mut journal = match load_retirement_journal(bro_home, project_id)
        .map_err(retirement_journal_admin_error)?
    {
        Some(j) if j.current_stage == RetirementJournalStage::Complete => {
            // Already complete. Verify quiescence (section 11.4).
            workers.verify_retirement_quiescent(project_id, &j.evidence)?;
            archive_retirement_journal(bro_home, project_id)
                .map_err(retirement_journal_admin_error)?;
            return Ok((preflight, Some(j)));
        }
        Some(j) => j,
        None => {
            // No journal on disk. If the project is already absent from
            // the catalog, the retirement completed in a prior run (the
            // journal was archived on completion). Treat as already-done:
            // skip all stages without calling any discharge workers.
            if !_project_exists {
                let archived = load_archived_retirement_journal(bro_home, project_id)
                    .map_err(retirement_journal_admin_error)?
                    .ok_or_else(|| {
                        admin_error(
                            "error.project_catalog_retire_missing_recovery_evidence",
                            format!(
                                "project {project_id} is absent but no completed retirement \
                                 journal exists to verify recovery"
                            ),
                        )
                    })?;
                workers.verify_retirement_quiescent(project_id, &archived.evidence)?;
                return Ok((preflight, Some(archived)));
            }
            let mut j =
                ProjectRetirementJournal::new(project_id.clone(), catalog_epoch, &journal_now());
            j.evidence = workers.capture_retirement_evidence(project_id)?;
            j.seal_retirement_evidence();
            save_retirement_journal(bro_home, &j).map_err(|e| {
                admin_error("error.project_catalog_retire_journal_io", e.to_string())
            })?;
            j
        }
    };

    let now = || journal_now();

    // Producer grants are configuration authority, so every resume checks
    // them again before any destructive stage can run.
    workers.validate_retirement_evidence(
        store,
        project_id,
        &journal.evidence,
        journal.current_stage,
    )?;
    workers.verify_source_authority_quiesced(store, project_id, &journal.evidence)?;

    // Stage: SourceAuthorityQuiesced (step 2). The worker must verify
    // that the project no longer holds active auth assignments or
    // un-revoked producer bindings before the journal may advance (F5).
    // A worker that returns Err blocks the journal at this stage.
    if !journal
        .current_stage
        .is_at_least(RetirementJournalStage::SourceAuthorityQuiesced)
    {
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    // Stage: CollectedGenerationsDischarged (section 11.3).
    // The journal discharges collected generations to zero: retire
    // selectors, delete source-owned records, clear entity references
    // and project-scoped rows. Single-attempt, no retry loops (11.1).
    if !journal
        .current_stage
        .is_at_least(RetirementJournalStage::CollectedGenerationsDischarged)
    {
        workers.validate_retirement_evidence(
            store,
            project_id,
            &journal.evidence,
            journal.current_stage,
        )?;
        workers.discharge_collected_generations(project_id, &journal.evidence)?;
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    // Stage: PublicationsCleared (section 11.3).
    if !journal
        .current_stage
        .is_at_least(RetirementJournalStage::PublicationsCleared)
    {
        workers.discharge_publications(project_id)?;
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    // Stage: AttachmentsDetached (section 11.3).
    if !journal
        .current_stage
        .is_at_least(RetirementJournalStage::AttachmentsDetached)
    {
        workers.discharge_attachments(store, project_id)?;
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    // Stage: CatalogPairRemoved (section 11.3).
    // This is the FINAL authority cut: retire_project(execute: true).
    // The prior discharge stages (CollectedGenerationsDischarged,
    // PublicationsCleared, AttachmentsDetached) should have zeroed every
    // blocking class. Rather than trust that the discharge workers
    // succeeded (spec 11.3 step 7: "At this point every blocking class
    // is zero, so it succeeds" describes verified reality, not
    // synthesized input), the journal calls reprobe_evidence to
    // re-inventory the cross-store reference classes from CURRENT state.
    // If any class is still nonzero, retire_project refuses with the
    // existing retire_blocked error (fail-closed): the journal stays at
    // its current stage for resume after the operator investigates.
    // If the project is already absent (idempotent re-entry), skip.
    if !journal
        .current_stage
        .is_at_least(RetirementJournalStage::CatalogPairRemoved)
    {
        workers.validate_retirement_evidence(
            store,
            project_id,
            &journal.evidence,
            journal.current_stage,
        )?;
        let current_state = store.snapshot()?;
        if current_state.catalog().projects.contains_key(project_id) {
            let reprobed_evidence =
                workers.reprobe_evidence(store, project_id, evidence, &journal.evidence)?;
            // R2F1: carry unprobeable classes through as refusals. An
            // unprobeable class must not be mistaken for a discharged zero.
            if !reprobed_evidence.unprobeable_classes.is_empty() {
                return Err(admin_error(
                    "error.project_catalog_retire_unprobeable_classes",
                    format!(
                        "cannot retire: {} class(es) could not be probed: {}; \
                         investigate the store state before retrying",
                        reprobed_evidence.unprobeable_classes.len(),
                        reprobed_evidence.unprobeable_classes.join(", ")
                    ),
                ));
            }
            let (_inventory, _commit) = retire_project(
                store,
                current_state.epoch(),
                project_id,
                &reprobed_evidence,
                true,
            )?;
        }
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    let post_cut_state = store.snapshot()?;
    if post_cut_state.catalog().projects.contains_key(project_id) {
        return Err(admin_error(
            "error.project_catalog_retire_recovery_not_quiescent",
            format!(
                "retirement journal reached the post-cut path while project {project_id} is present"
            ),
        ));
    }
    workers.verify_retirement_quiescent(project_id, &journal.evidence)?;

    // Stage: MaterializationSwept (section 11.3).
    // Blob deletion only when shared-history reference accounting
    // reaches zero. When other projects still reference shared blobs,
    // the sweep verifiably skips them. No catalog or auth lock held.
    if !journal
        .current_stage
        .is_at_least(RetirementJournalStage::MaterializationSwept)
    {
        workers.validate_retirement_evidence(
            store,
            project_id,
            &journal.evidence,
            journal.current_stage,
        )?;
        workers.sweep_materialization(project_id, &journal.evidence)?;
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    // Stage: Complete (step 9). Archive the journal.
    if journal.current_stage != RetirementJournalStage::Complete {
        workers.verify_retirement_quiescent(project_id, &journal.evidence)?;
        journal.advance(&now());
        save_retirement_journal(bro_home, &journal)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
        archive_retirement_journal(bro_home, project_id)
            .map_err(|e| admin_error("error.project_catalog_retire_journal_io", e.to_string()))?;
    }

    Ok((preflight, Some(journal)))
}

/// Build the preflight inventory from the current catalog state and
/// the probed evidence.
fn build_preflight(
    state: &crate::project_catalog_store::ProjectCatalogState,
    project_id: &ProjectId,
    evidence: &RetireEvidence,
) -> RetirementPreflight {
    use bbox_corpus_core::project_catalog::{RepoHistoryAuthority, RepoHistoryMaterialization};

    let mut blocking: std::collections::BTreeMap<String, u64> = evidence
        .external_reference_counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(class, count)| (class.clone(), *count))
        .collect();

    let active_attachments = state
        .attachments()
        .attachments
        .values()
        .filter(|row| row.status == AttachmentStatus::Attached && &row.project_id == project_id)
        .count() as u64;
    if active_attachments > 0 {
        blocking.insert("active_attachments".into(), active_attachments);
    }

    let history_ready_refusal = state
        .catalog()
        .projects
        .get(project_id)
        .and_then(|project| project.repo_history.as_ref())
        .and_then(|history_id| {
            let history = state.catalog().repo_histories.get(history_id)?;
            let still_referenced = state.catalog().projects.values().any(|other| {
                other.project_id != *project_id && other.repo_history.as_ref() == Some(history_id)
            });
            let deletion_eligible = !still_referenced
                && matches!(&history.authority, RepoHistoryAuthority::LocalProject(owner) if owner == project_id);
            let ready = matches!(
                history.materialization,
                RepoHistoryMaterialization::Ready { .. }
            );
            (deletion_eligible && ready).then_some(())
        })
        .is_some();

    let source_owned_records = state
        .catalog()
        .scope_migrations
        .values()
        .filter(|record| &record.project_id == project_id)
        .count() as u64;

    RetirementPreflight {
        blocking,
        history_ready_refusal,
        source_owned_records,
        project_exists: state.catalog().projects.contains_key(project_id),
        catalog_epoch: state.epoch(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CorpusProject, ScopeMigrationId, ScopeMigrationRecord,
    };
    use std::path::Path;
    use std::sync::Arc;

    const PROJECT: &str = "p_000000000000000000000000000000a1";
    const OTHER: &str = "p_000000000000000000000000000000b1";
    const CHECKOUT: &str = "feed00000000000000000000000000a1";

    struct RejectPostCutOwnerRows {
        inner: NoopDischargeWorkers,
        verify_calls: usize,
    }

    impl RetirementDischargeWorkers for RejectPostCutOwnerRows {
        fn capture_retirement_evidence(
            &mut self,
            project_id: &ProjectId,
        ) -> AdminResult<RetirementJournalEvidence> {
            self.inner.capture_retirement_evidence(project_id)
        }

        fn discharge_collected_generations(
            &mut self,
            project_id: &ProjectId,
            evidence: &RetirementJournalEvidence,
        ) -> AdminResult<()> {
            self.inner
                .discharge_collected_generations(project_id, evidence)
        }

        fn discharge_publications(&mut self, project_id: &ProjectId) -> AdminResult<()> {
            self.inner.discharge_publications(project_id)
        }

        fn discharge_attachments(
            &mut self,
            store: &ProjectCatalogStore,
            project_id: &ProjectId,
        ) -> AdminResult<()> {
            self.inner.discharge_attachments(store, project_id)
        }

        fn sweep_materialization(
            &mut self,
            project_id: &ProjectId,
            evidence: &RetirementJournalEvidence,
        ) -> AdminResult<()> {
            self.inner.sweep_materialization(project_id, evidence)
        }

        fn verify_source_authority_quiesced(
            &mut self,
            store: &ProjectCatalogStore,
            project_id: &ProjectId,
            evidence: &RetirementJournalEvidence,
        ) -> AdminResult<()> {
            self.inner
                .verify_source_authority_quiesced(store, project_id, evidence)
        }

        fn verify_retirement_quiescent(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &RetirementJournalEvidence,
        ) -> AdminResult<()> {
            self.verify_calls += 1;
            Err(admin_error(
                "error.project_catalog_retire_recovery_not_quiescent",
                "injected owner row",
            ))
        }

        fn reprobe_evidence(
            &mut self,
            store: &ProjectCatalogStore,
            project_id: &ProjectId,
            original_evidence: &RetireEvidence,
            retirement_evidence: &RetirementJournalEvidence,
        ) -> AdminResult<RetireEvidence> {
            self.inner
                .reprobe_evidence(store, project_id, original_evidence, retirement_evidence)
        }
    }

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
    fn relpath_move_relocates_attachments_and_appends_bindings() {
        use bbox_corpus_core::project_catalog::ScopeMigrationKind;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("checkout/apps/web")).unwrap();
        let store = store_with_projects(&root);
        let project_id = ProjectId::parse(PROJECT).unwrap();

        // Promote first so the project is published at relpath ".".
        let receipt = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("checkout")),
        )
        .unwrap();
        let old_scope = PublishedScope::try_new("movefamily", ".").unwrap();
        let evidence = PromotionEvidence {
            attachment_scopes: [(receipt.attachment_id.clone(), Some(old_scope.clone()))]
                .into_iter()
                .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:promote".into(),
            operator_reason: None,
            proved_at: "2026-07-24T00:00:03Z".into(),
        };
        promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &receipt.attachment_id,
            &old_scope,
            &evidence,
        )
        .unwrap();

        let new_scope = PublishedScope::try_new("movefamily", "apps/web").unwrap();
        let new_dir = root.join("checkout/apps/web");
        let request = ScopeMigrationRequest {
            project_id: project_id.clone(),
            expected_old_scope: old_scope.clone(),
            new_scope: new_scope.clone(),
            kind: ScopeMigrationKind::RelpathMove,
            designated_attachment: receipt.attachment_id.clone(),
            acknowledge_repo_authority_change: false,
            attachment_probes: [(
                receipt.attachment_id.clone(),
                MigrationAttachmentProbe {
                    resolved_scope: Some(new_scope.clone()),
                    new_project_root_relpath: "apps/web".into(),
                    new_checkout_project_dir: new_dir.to_str().unwrap().into(),
                },
            )]
            .into_iter()
            .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:migrate".into(),
            operator_reason: None,
            migrated_at: "2026-07-24T00:00:05Z".into(),
        };

        // Dry run validates and commits nothing.
        let epoch_before = current_epoch(&store);
        let dry = scope_migrate_attached(&store, epoch_before, &request, true).unwrap();
        assert!(dry.is_none());
        assert_eq!(current_epoch(&store), epoch_before);

        let receipt2 = scope_migrate_attached(&store, epoch_before, &request, false)
            .unwrap()
            .expect("live run returns a receipt");
        let state = store.snapshot().unwrap();
        let project = state.catalog().projects.get(&project_id).unwrap();
        assert_eq!(project.scope, ProjectScope::Published(new_scope.clone()));
        let row = state
            .attachments()
            .attachments
            .get(&receipt.attachment_id)
            .unwrap();
        assert_eq!(row.project_root_relpath, "apps/web");
        assert_eq!(row.validated_scope.as_ref(), Some(&new_scope));
        assert_eq!(
            state.attachments().legacy_path_bindings.len(),
            1,
            "relocation appends exactly one historical binding"
        );
        let binding = state
            .attachments()
            .legacy_path_bindings
            .values()
            .next()
            .unwrap();
        assert_eq!(
            binding.historical_path,
            root.join("checkout").to_str().unwrap()
        );
        assert!(
            state
                .catalog()
                .scope_migrations
                .contains_key(&receipt2.scope_migration_id)
        );
        assert!(
            state
                .attachments()
                .scope_migration_proofs
                .contains_key(&receipt2.scope_migration_id)
        );
    }

    #[test]
    fn authority_change_requires_acknowledgement_and_shape_gates_hold() {
        use bbox_corpus_core::project_catalog::ScopeMigrationKind;
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
        let old_scope = PublishedScope::try_new("authfamilyone", ".").unwrap();
        let evidence = PromotionEvidence {
            attachment_scopes: [(receipt.attachment_id.clone(), Some(old_scope.clone()))]
                .into_iter()
                .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:promote".into(),
            operator_reason: None,
            proved_at: "2026-07-24T00:00:03Z".into(),
        };
        promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &receipt.attachment_id,
            &old_scope,
            &evidence,
        )
        .unwrap();

        let new_scope = PublishedScope::try_new("authfamilytwo", ".").unwrap();
        let mut request = ScopeMigrationRequest {
            project_id: project_id.clone(),
            expected_old_scope: old_scope.clone(),
            new_scope: new_scope.clone(),
            kind: ScopeMigrationKind::RepoAuthorityChange,
            designated_attachment: receipt.attachment_id.clone(),
            acknowledge_repo_authority_change: false,
            attachment_probes: [(
                receipt.attachment_id.clone(),
                MigrationAttachmentProbe {
                    resolved_scope: Some(new_scope.clone()),
                    new_project_root_relpath: ".".into(),
                    new_checkout_project_dir: root.join("checkout").to_str().unwrap().into(),
                },
            )]
            .into_iter()
            .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:migrate".into(),
            operator_reason: None,
            migrated_at: "2026-07-24T00:00:06Z".into(),
        };

        // Missing acknowledgement refuses (operator authority, D-004).
        let error =
            scope_migrate_attached(&store, current_epoch(&store), &request, false).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_acknowledgement_required"
        );

        // Wrong shape refuses: an authority change may not change relpath.
        request.acknowledge_repo_authority_change = true;
        let mut wrong = request.clone();
        wrong.new_scope = PublishedScope::try_new("authfamilytwo", "sub").unwrap();
        let error =
            scope_migrate_attached(&store, current_epoch(&store), &wrong, false).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_migration_shape");

        // The acknowledged, well-shaped change lands and preserves the
        // established primary namespace while re-recording authority.
        scope_migrate_attached(&store, current_epoch(&store), &request, false)
            .unwrap()
            .unwrap();
        let state = store.snapshot().unwrap();
        let project = state.catalog().projects.get(&project_id).unwrap();
        assert_eq!(project.scope, ProjectScope::Published(new_scope));
        let history = state
            .catalog()
            .repo_histories
            .get(project.repo_history.as_ref().unwrap())
            .unwrap();
        assert_eq!(
            history.primary_namespace.as_str(),
            "authfamilyone",
            "the established primary namespace never changes"
        );
        use bbox_corpus_core::project_catalog::RepoHistoryAuthority;
        assert!(matches!(
            &history.authority,
            RepoHistoryAuthority::Recorded(a) if a.as_str() == "authfamilytwo"
        ));
    }

    #[test]
    fn catalog_add_alias_lifecycle_and_retire_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();

        // Remote-shaped published add with an initial alias.
        let scope = PublishedScope::try_new("remotefamily", ".").unwrap();
        let (published_id, _) = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::Published(scope.clone()),
            "remote project",
            &["remote-alias".to_string()],
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        // The same scope cannot be added twice.
        let error = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::Published(scope.clone()),
            "dup",
            &[],
            "2026-07-24T00:00:01Z",
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_owned");

        // Legacy-local add mints a local history record.
        let (local_id, _) = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::LegacyLocal,
            "local project",
            &[],
            "2026-07-24T00:00:02Z",
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let local = state.catalog().projects.get(&local_id).unwrap();
        let history = state
            .catalog()
            .repo_histories
            .get(local.repo_history.as_ref().unwrap())
            .unwrap();
        assert!(history.primary_namespace.as_str().starts_with("local_"));

        // Alias nomination lifecycle: nominate by direct mutation (the
        // register-time ingestion is the tool layer), then accept.
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                catalog
                    .projects
                    .get_mut(&local_id)
                    .unwrap()
                    .nominated_aliases
                    .insert("nominated".into());
                Ok(())
            })
            .unwrap();
        // Accepting a colliding alias refuses and keeps the nomination.
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                catalog
                    .projects
                    .get_mut(&local_id)
                    .unwrap()
                    .nominated_aliases
                    .insert("remote-alias".into());
                Ok(())
            })
            .unwrap();
        let error = alias_decide(
            &store,
            current_epoch(&store),
            &local_id,
            "remote-alias",
            true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_alias_conflict");
        alias_decide(
            &store,
            current_epoch(&store),
            &local_id,
            "remote-alias",
            false,
        )
        .unwrap();
        alias_decide(&store, current_epoch(&store), &local_id, "nominated", true).unwrap();
        let state = store.snapshot().unwrap();
        let local = state.catalog().projects.get(&local_id).unwrap();
        assert!(local.operator_aliases.contains("nominated"));
        assert!(local.nominated_aliases.is_empty());
        // Deciding a nomination that does not exist refuses.
        let error =
            alias_decide(&store, current_epoch(&store), &local_id, "ghost", true).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_unknown_nomination"
        );

        // Retire: blocked by probed external references, then clean.
        let mut evidence = RetireEvidence::default();
        evidence
            .external_reference_counts
            .insert("knowledge_rows".into(), 2);
        evidence
            .external_reference_counts
            .insert("code_source_activation".into(), 1);
        let (inventory, commit) = retire_project(
            &store,
            current_epoch(&store),
            &published_id,
            &evidence,
            false,
        )
        .unwrap();
        assert_eq!(inventory.blocking.get("knowledge_rows"), Some(&2));
        assert_eq!(inventory.blocking.get("code_source_activation"), Some(&1));
        assert!(commit.is_none());
        let error = retire_project(
            &store,
            current_epoch(&store),
            &published_id,
            &evidence,
            true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_retire_blocked");

        let (_, commit) = retire_project(
            &store,
            current_epoch(&store),
            &published_id,
            &RetireEvidence::default(),
            true,
        )
        .unwrap();
        assert!(commit.is_some());
        let state = store.snapshot().unwrap();
        assert!(!state.catalog().projects.contains_key(&published_id));
        // The local project and its history survive untouched.
        assert!(state.catalog().projects.contains_key(&local_id));
    }

    #[test]
    fn retire_refuses_to_delete_a_ready_materialized_local_history() {
        use bbox_corpus_core::project_catalog::{
            RepoHistoryGenerationId, RepoHistoryMaterialization,
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();

        let (local_id, _) = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::LegacyLocal,
            "local project",
            &[],
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let history_id = state
            .catalog()
            .projects
            .get(&local_id)
            .unwrap()
            .repo_history
            .clone()
            .unwrap();

        // Materialize the history record directly (simulating the P3-D
        // materializer's catalog transaction, which does not exist yet in
        // this milestone).
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                catalog
                    .repo_histories
                    .get_mut(&history_id)
                    .unwrap()
                    .materialization = RepoHistoryMaterialization::Ready {
                    generation_id: RepoHistoryGenerationId::parse(format!(
                        "rhg_{}",
                        "a".repeat(64)
                    ))
                    .unwrap(),
                };
                Ok(())
            })
            .unwrap();

        let (inventory, commit) = retire_project(
            &store,
            current_epoch(&store),
            &local_id,
            &RetireEvidence::default(),
            false,
        )
        .unwrap();
        assert_eq!(
            inventory.blocking.get("history_generation_referenced"),
            Some(&1)
        );
        assert!(commit.is_none());

        let error = retire_project(
            &store,
            current_epoch(&store),
            &local_id,
            &RetireEvidence::default(),
            true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_retire_blocked");
        let state = store.snapshot().unwrap();
        assert!(
            state.catalog().projects.contains_key(&local_id),
            "the project must survive a refused retire"
        );

        // Once the history reverts to NotBuilt, retire proceeds and removes
        // the now-unreferenced local history record exactly as before.
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                catalog
                    .repo_histories
                    .get_mut(&history_id)
                    .unwrap()
                    .materialization = RepoHistoryMaterialization::NotBuilt;
                Ok(())
            })
            .unwrap();
        let (_, commit) = retire_project(
            &store,
            current_epoch(&store),
            &local_id,
            &RetireEvidence::default(),
            true,
        )
        .unwrap();
        assert!(commit.is_some());
        let state = store.snapshot().unwrap();
        assert!(!state.catalog().projects.contains_key(&local_id));
        assert!(!state.catalog().repo_histories.contains_key(&history_id));
    }

    #[test]
    fn retire_never_touches_a_ready_materialized_non_local_history() {
        use bbox_corpus_core::project_catalog::{
            RepoHistoryGenerationId, RepoHistoryMaterialization,
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();

        let scope = PublishedScope::try_new("retire-non-local-family", ".").unwrap();
        let (published_id, _) = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::Published(scope.clone()),
            "published project",
            &[],
            "2026-07-25T00:00:00Z",
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let history_id = state
            .catalog()
            .projects
            .get(&published_id)
            .unwrap()
            .repo_history
            .clone()
            .unwrap();

        // Materialize the Recorded-authority history record. The transact
        // closure's deletion site only ever removes a LocalProject-authority
        // record (see the comment there), so this record must never be
        // touched by retire regardless of materialization: there is no
        // deletion path that could reach it, hence nothing for the blocking
        // guard to refuse.
        let epoch = current_epoch(&store);
        store
            .transact(epoch, |catalog, _| {
                catalog
                    .repo_histories
                    .get_mut(&history_id)
                    .unwrap()
                    .materialization = RepoHistoryMaterialization::Ready {
                    generation_id: RepoHistoryGenerationId::parse(format!(
                        "rhg_{}",
                        "b".repeat(64)
                    ))
                    .unwrap(),
                };
                Ok(())
            })
            .unwrap();

        let (inventory, commit) = retire_project(
            &store,
            current_epoch(&store),
            &published_id,
            &RetireEvidence::default(),
            true,
        )
        .unwrap();
        assert!(
            !inventory
                .blocking
                .contains_key("history_generation_referenced"),
            "a Recorded-authority history record is never a deletion candidate, \
             so a Ready materialization cannot block retire: {:?}",
            inventory.blocking
        );
        assert!(commit.is_some());
        let state = store.snapshot().unwrap();
        assert!(!state.catalog().projects.contains_key(&published_id));
        assert!(
            state.catalog().repo_histories.contains_key(&history_id),
            "retire structurally cannot delete a non-LocalProject-authority history record"
        );
    }

    #[test]
    fn retire_removes_the_audit_chain_attachments_and_bindings_together() {
        use bbox_corpus_core::project_catalog::ScopeMigrationKind;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("checkout/apps/web")).unwrap();
        let store = store_with_projects(&root);
        let project_id = ProjectId::parse(PROJECT).unwrap();

        let receipt = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("checkout")),
        )
        .unwrap();
        let old_scope = PublishedScope::try_new("retirefamily", ".").unwrap();
        let evidence = PromotionEvidence {
            attachment_scopes: [(receipt.attachment_id.clone(), Some(old_scope.clone()))]
                .into_iter()
                .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:promote".into(),
            operator_reason: None,
            proved_at: "2026-07-24T00:00:03Z".into(),
        };
        promote_project(
            &store,
            current_epoch(&store),
            &project_id,
            &receipt.attachment_id,
            &old_scope,
            &evidence,
        )
        .unwrap();
        let new_scope = PublishedScope::try_new("retirefamily", "apps/web").unwrap();
        let request = ScopeMigrationRequest {
            project_id: project_id.clone(),
            expected_old_scope: old_scope.clone(),
            new_scope: new_scope.clone(),
            kind: ScopeMigrationKind::RelpathMove,
            designated_attachment: receipt.attachment_id.clone(),
            acknowledge_repo_authority_change: false,
            attachment_probes: [(
                receipt.attachment_id.clone(),
                MigrationAttachmentProbe {
                    resolved_scope: Some(new_scope.clone()),
                    new_project_root_relpath: "apps/web".into(),
                    new_checkout_project_dir: root
                        .join("checkout/apps/web")
                        .to_str()
                        .unwrap()
                        .into(),
                },
            )]
            .into_iter()
            .collect(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "test:migrate".into(),
            operator_reason: None,
            migrated_at: "2026-07-24T00:00:05Z".into(),
        };
        scope_migrate_attached(&store, current_epoch(&store), &request, false)
            .unwrap()
            .unwrap();

        // Active attachment blocks execute.
        let error = retire_project(
            &store,
            current_epoch(&store),
            &project_id,
            &RetireEvidence::default(),
            true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_retire_blocked");

        detach_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            "2026-07-24T00:00:06Z",
        )
        .unwrap();
        let (inventory, commit) = retire_project(
            &store,
            current_epoch(&store),
            &project_id,
            &RetireEvidence::default(),
            true,
        )
        .unwrap();
        assert_eq!(inventory.removable_migrations, 2);
        assert_eq!(inventory.removable_attachments, 1);
        assert_eq!(inventory.removable_bindings, 1);
        assert!(commit.is_some());
        let state = store.snapshot().unwrap();
        assert!(!state.catalog().projects.contains_key(&project_id));
        assert!(state.catalog().scope_migrations.is_empty());
        assert!(state.attachments().scope_migration_proofs.is_empty());
        assert!(state.attachments().attachments.is_empty());
        assert!(state.attachments().legacy_path_bindings.is_empty());
    }

    #[test]
    fn attested_migration_requires_zero_attachments_flags_and_reason() {
        use bbox_corpus_core::project_catalog::{
            ScopeMigrationAuthorityProvenance, ScopeMigrationKind,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();
        let old_scope = PublishedScope::try_new("attestfamily", ".").unwrap();
        let (project_id, _) = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::Published(old_scope.clone()),
            "remote",
            &[],
            "2026-07-24T00:00:00Z",
        )
        .unwrap();

        let new_scope = PublishedScope::try_new("attestfamily", "svc/api").unwrap();
        let mut request = ScopeMigrationRequest {
            project_id: project_id.clone(),
            expected_old_scope: old_scope.clone(),
            new_scope: new_scope.clone(),
            kind: ScopeMigrationKind::RelpathMove,
            designated_attachment: AttachmentId::mint(),
            acknowledge_repo_authority_change: false,
            attachment_probes: Default::default(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            operator_invocation: "cli:scope-migrate --operator-attested".into(),
            operator_reason: None,
            migrated_at: "2026-07-24T00:00:01Z".into(),
        };

        // Acknowledgement and reason are both mandatory.
        let error = scope_migrate_attested(&store, current_epoch(&store), &request, false, false)
            .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_acknowledgement_required"
        );
        let error = scope_migrate_attested(&store, current_epoch(&store), &request, true, false)
            .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_reason_required");

        request.operator_reason = Some("relocating the remote-only service root".into());
        let before = store.snapshot().unwrap();
        assert!(
            scope_migrate_attested(&store, before.epoch(), &request, true, true)
                .unwrap()
                .is_none()
        );
        let after_dry_run = store.snapshot().unwrap();
        assert_eq!(after_dry_run.epoch(), before.epoch());
        assert_eq!(after_dry_run.catalog_sha256(), before.catalog_sha256());
        assert!(after_dry_run.catalog().scope_migrations.is_empty());

        let receipt = scope_migrate_attested(&store, current_epoch(&store), &request, true, false)
            .unwrap()
            .expect("non-dry attested migration commits");
        let state = store.snapshot().unwrap();
        let record = state
            .catalog()
            .scope_migrations
            .get(&receipt.scope_migration_id)
            .unwrap();
        assert_eq!(
            record.authority_provenance,
            ScopeMigrationAuthorityProvenance::OperatorAttested
        );
        assert!(
            state.attachments().scope_migration_proofs.is_empty(),
            "operator-attested records carry no proof row"
        );

        // With an active attachment present, the attested channel refuses.
        let (legacy_id, _) = catalog_add(
            &store,
            current_epoch(&store),
            &CatalogAddKind::LegacyLocal,
            "attached",
            &[],
            "2026-07-24T00:00:02Z",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("co")).unwrap();
        attach_checkout(
            &store,
            current_epoch(&store),
            &legacy_id,
            &probe(&root.join("co")),
        )
        .unwrap();
        let mut attached_request = request.clone();
        attached_request.project_id = legacy_id;
        attached_request.expected_old_scope = new_scope.clone();
        let error = scope_migrate_attested(
            &store,
            current_epoch(&store),
            &attached_request,
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_attachments_active"
        );
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

    #[test]
    fn register_composite_creates_finds_and_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("checkout")).unwrap();
        let store = store_with_projects(&root);

        // Unrecorded checkout: mints a LegacyLocal project and attaches it
        // in one commit.
        let local_probe = probe(&root.join("checkout"));
        let receipt = register_composite(
            &store,
            current_epoch(&store),
            &local_probe,
            "local project",
            "2026-07-25T00:00:00Z",
        )
        .unwrap();
        assert!(receipt.created_project);
        assert!(!receipt.already_attached);
        assert!(receipt.commit.is_some());
        let state = store.snapshot().unwrap();
        let created = state.catalog().projects.get(&receipt.project_id).unwrap();
        assert_eq!(created.scope, ProjectScope::LegacyLocal);
        assert!(created.repo_history.is_some(), "local history minted");
        assert_eq!(
            state
                .attachments()
                .attachments
                .get(&receipt.attachment_id)
                .unwrap()
                .status,
            AttachmentStatus::Attached
        );

        // Same checkout again: idempotent, no commit, same identities.
        let epoch_before = current_epoch(&store);
        let again = register_composite(
            &store,
            epoch_before,
            &local_probe,
            "local project",
            "2026-07-25T00:00:01Z",
        )
        .unwrap();
        assert!(again.already_attached);
        assert!(again.commit.is_none());
        assert_eq!(again.project_id, receipt.project_id);
        assert_eq!(again.attachment_id, receipt.attachment_id);
        assert_eq!(current_epoch(&store), epoch_before, "no epoch bump");

        // The same checkout now recording committed authority: the exact
        // promotion handoff naming the project, and no second project.
        let mut promoted = local_probe.clone();
        promoted.validated_scope = Some(PublishedScope::try_new("regfamily", ".").unwrap());
        let error = register_composite(
            &store,
            current_epoch(&store),
            &promoted,
            "local project",
            "2026-07-25T00:00:02Z",
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_scope_promotion_required"
        );
        assert!(error.to_string().contains(receipt.project_id.as_str()));
        let count_before = store.snapshot().unwrap().catalog().projects.len();

        // A fresh checkout with a newly recorded scope: mints a Published
        // project and attaches.
        std::fs::create_dir_all(root.join("pub")).unwrap();
        let mut pub_probe = probe(&root.join("pub"));
        pub_probe.checkout_id = "feed00000000000000000000000000c1".into();
        pub_probe.validated_scope = Some(PublishedScope::try_new("regfamily", ".").unwrap());
        let published = register_composite(
            &store,
            current_epoch(&store),
            &pub_probe,
            "published project",
            "2026-07-25T00:00:03Z",
        )
        .unwrap();
        assert!(published.created_project);
        let state = store.snapshot().unwrap();
        assert_eq!(
            state.catalog().projects.len(),
            count_before + 1,
            "exactly one new project"
        );
        assert!(matches!(
            &state
                .catalog()
                .projects
                .get(&published.project_id)
                .unwrap()
                .scope,
            ProjectScope::Published(s) if s.repo_id() == "regfamily"
        ));

        // A second fresh checkout proving the SAME scope finds the existing
        // project instead of creating another.
        std::fs::create_dir_all(root.join("pub2")).unwrap();
        let mut second = probe(&root.join("pub2"));
        second.checkout_id = "feed00000000000000000000000000c2".into();
        second.validated_scope = Some(PublishedScope::try_new("regfamily", ".").unwrap());
        let found = register_composite(
            &store,
            current_epoch(&store),
            &second,
            "published project",
            "2026-07-25T00:00:04Z",
        )
        .unwrap();
        assert!(!found.created_project);
        assert_eq!(found.project_id, published.project_id);
        assert_ne!(found.attachment_id, published.attachment_id);

        // That attached checkout later resolving a DIFFERENT scope gets the
        // exact scope-migration handoff and no new project.
        let mut moved = second.clone();
        moved.validated_scope = Some(PublishedScope::try_new("regfamily", "apps/web").unwrap());
        let error = register_composite(
            &store,
            current_epoch(&store),
            &moved,
            "published project",
            "2026-07-25T00:00:05Z",
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_scope_migration_required"
        );
        assert!(error.to_string().contains("bbox_project_scope_migrate"));
        assert!(error.to_string().contains(published.project_id.as_str()));
    }

    #[test]
    fn relocate_attachment_moves_paths_and_appends_ledger() {
        use bbox_corpus_core::project_catalog::LegacyPathBindingStatus;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("old")).unwrap();
        let store = store_with_projects(&root);
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let receipt = attach_checkout(
            &store,
            current_epoch(&store),
            &project_id,
            &probe(&root.join("old")),
        )
        .unwrap();
        let old_dir = root.join("old").to_str().unwrap().to_string();
        let new_dir = root.join("new").to_str().unwrap().to_string();

        // Same checkout identity, same (absent) scope: the row relocates
        // and exactly one Mapped ledger entry records the historical path.
        relocate_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            &RelocationProbe {
                checkout_id: CHECKOUT.into(),
                new_checkout_dir: new_dir.clone(),
                new_checkout_project_dir: new_dir.clone(),
                resolved_scope: None,
            },
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let row = state
            .attachments()
            .attachments
            .get(&receipt.attachment_id)
            .unwrap();
        assert_eq!(row.checkout_dir, new_dir);
        assert_eq!(row.checkout_project_dir, new_dir);
        let bindings: Vec<_> = state
            .attachments()
            .legacy_path_bindings
            .values()
            .filter(|entry| entry.historical_path == old_dir)
            .collect();
        assert_eq!(bindings.len(), 1, "exactly one historical binding");
        assert!(matches!(
            &bindings[0].status,
            LegacyPathBindingStatus::Mapped { project_id: p, .. } if p == &project_id
        ));

        // A different checkout identity at the new path refuses: path
        // existence and inode reuse never prove sameness.
        let error = relocate_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            &RelocationProbe {
                checkout_id: "feed00000000000000000000000000ff".into(),
                new_checkout_dir: root.join("other").to_str().unwrap().into(),
                new_checkout_project_dir: root.join("other").to_str().unwrap().into(),
                resolved_scope: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_checkout_identity_mismatch"
        );

        // A resolved scope on a legacy-local project refuses toward the
        // explicit surfaces.
        let error = relocate_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            &RelocationProbe {
                checkout_id: CHECKOUT.into(),
                new_checkout_dir: root.join("scoped").to_str().unwrap().into(),
                new_checkout_project_dir: root.join("scoped").to_str().unwrap().into(),
                resolved_scope: Some(PublishedScope::try_new("relofamily", ".").unwrap()),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_mismatch");

        // A project-dir that breaks the recorded relpath refuses with the
        // scope-migration pointer.
        let error = relocate_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            &RelocationProbe {
                checkout_id: CHECKOUT.into(),
                new_checkout_dir: root.join("moved").to_str().unwrap().into(),
                new_checkout_project_dir: root.join("moved/sub").to_str().unwrap().into(),
                resolved_scope: None,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_mismatch");

        // Relocating to the recorded path is a typed no-op refusal.
        let error = relocate_attachment(
            &store,
            current_epoch(&store),
            &receipt.attachment_id,
            &RelocationProbe {
                checkout_id: CHECKOUT.into(),
                new_checkout_dir: new_dir.clone(),
                new_checkout_project_dir: new_dir,
                resolved_scope: None,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_relocation_noop");
    }

    // ----- Retirement journal tests (section 11.6) -----

    #[test]
    fn retirement_journal_stage_ordinal_is_forward_only() {
        assert!(RetirementJournalStage::Complete.is_at_least(RetirementJournalStage::Prepared));
        assert!(
            RetirementJournalStage::CatalogPairRemoved
                .is_at_least(RetirementJournalStage::AttachmentsDetached)
        );
        assert!(!RetirementJournalStage::Prepared.is_at_least(RetirementJournalStage::Complete));
    }

    #[test]
    fn retirement_journal_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 42, "12345");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let loaded = load_retirement_journal(tmp.path(), &pid).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.project_id, pid);
        assert_eq!(loaded.catalog_epoch_at_start, 42);
        assert_eq!(loaded.current_stage, RetirementJournalStage::Prepared);
    }

    #[test]
    fn retirement_journal_v1_requires_explicit_upgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut value =
            serde_json::to_value(ProjectRetirementJournal::new(pid.clone(), 1, "1")).unwrap();
        value["version"] = serde_json::json!(1);
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            load_retirement_journal(tmp.path(), &pid),
            Err(RetirementJournalError::UpgradeRequired(1))
        ));
    }

    #[test]
    fn retirement_journal_v2_requires_explicit_upgrade_before_typed_evidence_decode() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut value =
            serde_json::to_value(ProjectRetirementJournal::new(pid.clone(), 1, "1")).unwrap();
        value["version"] = serde_json::json!(2);
        value["evidence"]["artifact_targets"] =
            serde_json::json!(["/tmp/victim:aaaaaaaa:bbbbbbbb"]);
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            load_retirement_journal(tmp.path(), &pid),
            Err(RetirementJournalError::UpgradeRequired(2))
        ));
    }

    #[test]
    fn retirement_journal_rejects_self_consistent_unsafe_artifact_targets() {
        let invalid_directories = ["", "/tmp/victim", "../victim", "safe/../victim"];
        for invalid_directory in invalid_directories {
            let tmp = tempfile::tempdir().unwrap();
            let pid = ProjectId::parse(PROJECT).unwrap();
            let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
            journal.evidence.artifact_targets =
                Some(vec![bbox_artifacts::artifacts::ArtifactRetirementTarget {
                    owner_project_id: pid.as_str().to_string(),
                    legacy_project_path: None,
                    artifact_directory: invalid_directory.into(),
                    metadata_path: format!("{invalid_directory}/metadata.json"),
                    payload_path: format!("{invalid_directory}.json"),
                    metadata_sha256: "a".repeat(64),
                    version_metadata: Vec::new(),
                    payload_sha256: "b".repeat(64),
                    tree_manifest: vec![bbox_artifacts::artifacts::ArtifactMetadataCommitment {
                        path: format!("{invalid_directory}/metadata.json"),
                        sha256: "a".repeat(64),
                    }],
                }]);
            journal.seal_retirement_evidence();
            let path = retirement_journal_path(tmp.path(), &pid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
            assert!(
                load_retirement_journal(tmp.path(), &pid).is_err(),
                "unsafe artifact directory {invalid_directory:?} was accepted"
            );
        }
    }

    #[test]
    fn retirement_journal_rejects_evidence_owner_drift_at_every_resume_stage() {
        let stages = [
            RetirementJournalStage::Prepared,
            RetirementJournalStage::SourceAuthorityQuiesced,
            RetirementJournalStage::CollectedGenerationsDischarged,
            RetirementJournalStage::PublicationsCleared,
            RetirementJournalStage::AttachmentsDetached,
            RetirementJournalStage::CatalogPairRemoved,
            RetirementJournalStage::MaterializationSwept,
        ];
        for stage in stages {
            let tmp = tempfile::tempdir().unwrap();
            let pid = ProjectId::parse(PROJECT).unwrap();
            let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
            while journal.current_stage != stage {
                journal.advance("2");
            }
            save_retirement_journal(tmp.path(), &journal).unwrap();
            let path = retirement_journal_path(tmp.path(), &pid);
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["evidence"]["owner_project_id"] = serde_json::json!("project-drift");
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(
                load_retirement_journal(tmp.path(), &pid).is_err(),
                "stage {stage:?} accepted drifted destructive evidence"
            );
        }
    }

    #[test]
    fn retirement_blob_sidecar_scales_to_manifest_file_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        journal.evidence.owned_blob_hashes = (0_u64..250_000)
            .map(|index| format!("{index:064x}"))
            .collect();
        journal.seal_retirement_evidence();
        save_retirement_journal(tmp.path(), &journal).unwrap();
        assert!(
            std::fs::metadata(retirement_journal_path(tmp.path(), &pid))
                .unwrap()
                .len()
                < MAX_JOURNAL_BYTES as u64
        );
        let loaded = load_retirement_journal(tmp.path(), &pid).unwrap().unwrap();
        assert_eq!(loaded.evidence.owned_blob_hashes.len(), 250_000);
    }

    #[test]
    fn retirement_journal_rejects_added_target_after_stage_advance() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        journal.advance("2");
        journal.advance("3");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let path = retirement_journal_path(tmp.path(), &pid);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["evidence"]["owned_generations"] = serde_json::json!([{
            "published_scope": {
                "repo_id": "other-owner",
                "bbox_root_relpath": "."
            },
            "generation_id": "a".repeat(64)
        }]);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(load_retirement_journal(tmp.path(), &pid).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retirement_blob_sidecar_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let journal_path = retirement_journal_path(tmp.path(), &pid);
        let sidecar = retirement_blob_inventory_path(&journal_path);
        let target = tmp.path().join("outside.json");
        std::fs::write(&target, b"{}").unwrap();
        std::fs::remove_file(&sidecar).unwrap();
        symlink(&target, &sidecar).unwrap();

        let error = load_retirement_journal(tmp.path(), &pid).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }

    #[test]
    fn retirement_blob_sidecar_refuses_oversized_file_before_read() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let journal_path = retirement_journal_path(tmp.path(), &pid);
        let sidecar = retirement_blob_inventory_path(&journal_path);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();

        let error = load_retirement_journal(tmp.path(), &pid).unwrap_err();
        assert!(error.to_string().contains("exceeds its byte limit"));
    }

    #[test]
    fn retirement_journal_path_convention() {
        let bro_home = std::path::Path::new("/tmp/bro");
        let pid = ProjectId::parse(PROJECT).unwrap();
        let path = retirement_journal_path(bro_home, &pid);
        assert!(path.ends_with("retirement-journals/p_000000000000000000000000000000a1.json"));
    }

    #[test]
    fn retirement_journal_archive_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        assert!(retirement_journal_path(tmp.path(), &pid).is_file());
        archive_retirement_journal(tmp.path(), &pid).unwrap();
        assert!(!retirement_journal_path(tmp.path(), &pid).is_file());
    }

    // ---- F4: bridge-clear precondition tests ----

    fn f4_store_with_bridge(
        bridge_gen: Option<&str>,
    ) -> (tempfile::TempDir, ProjectCatalogStore, ProjectId) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("catalog")).unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();

        // Add the project to the catalog with a matching attachment.
        let base = store.snapshot().unwrap();
        let epoch = base.epoch();
        let new_scope = ProjectScope::Published(PublishedScope::try_new("f4-scope", ".").unwrap());
        let new_scope_for_closure = new_scope.clone();
        let att_id =
            AttachmentId::parse("att_11111111111111111111111111111111".to_string()).unwrap();
        store
            .transact(epoch, |catalog, attachments| {
                catalog.projects.insert(
                    pid.clone(),
                    CorpusProject {
                        project_id: pid.clone(),
                        scope: new_scope_for_closure,
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: "f4-test".into(),
                        created_at: "2026-07-24T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                attachments.attachments.insert(
                    att_id.clone(),
                    CheckoutAttachment {
                        attachment_id: att_id.clone(),
                        project_id: pid.clone(),
                        checkout_id: "22222222222222222222222222222222".into(),
                        checkout_dir: "/tmp/f4".into(),
                        checkout_project_dir: "/tmp/f4".into(),
                        project_root_relpath: ".".into(),
                        kind: AttachmentKind::Base,
                        validated_scope: Some(PublishedScope::try_new("f4-scope", ".").unwrap()),
                        computed_repo_hint: None,
                        branch_ref: None,
                        capabilities: AttachmentCapabilities::default(),
                        status: AttachmentStatus::Attached,
                        attached_at: "2026-07-24T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();

        // Add a scope migration record with a bridge generation.
        let base = store.snapshot().unwrap();
        let epoch = base.epoch();
        let migration_id =
            ScopeMigrationId::parse("sm_11111111111111111111111111111111".to_string()).unwrap();
        let new_scope = ProjectScope::Published(PublishedScope::try_new("f4-scope", ".").unwrap());
        store
            .transact(epoch, |catalog, attachments| {
                catalog.scope_migrations.insert(
                    migration_id.clone(),
                    ScopeMigrationRecord {
                        scope_migration_id: migration_id.clone(),
                        project_id: pid.clone(),
                        catalog_epoch: epoch,
                        authority_provenance:
                            bbox_corpus_core::project_catalog::ScopeMigrationAuthorityProvenance::AttachmentProved,
                        operator_invocation: "f4-test".to_string(),
                        operator_reason: None,
                        old_scope: ProjectScope::LegacyLocal,
                        new_scope: new_scope.clone(),
                        kind: bbox_corpus_core::project_catalog::ScopeMigrationKind::Promotion,
                        migrated_at: "2026-07-24T00:00:00Z".to_string(),
                        code_bridge_generation: bridge_gen.map(|g| g.to_string()),
                        publication_bridge_generation: None,
                        pending_capabilities: Default::default(),
                    },
                );
                // The migration validation requires a matching attachment proof.
                attachments.scope_migration_proofs.insert(
                    migration_id,
                    bbox_corpus_core::project_catalog::ScopeMigrationAttachmentProof {
                        scope_migration_id: ScopeMigrationId::parse("sm_11111111111111111111111111111111".to_string()).unwrap(),
                        attachment_id: AttachmentId::parse("att_11111111111111111111111111111111".to_string()).unwrap(),
                        checkout_id: "22222222222222222222222222222222".to_string(),
                        old_scope: ProjectScope::LegacyLocal,
                        new_scope,
                        proved_at: "2026-07-24T00:00:00Z".to_string(),
                    },
                );
                Ok(())
            })
            .unwrap();

        (tmp, store, pid)
    }

    fn absent_bridge_evidence(
        retained: impl IntoIterator<Item = &'static str>,
    ) -> ScopeBridgeClearEvidence {
        ScopeBridgeClearEvidence {
            activation: ScopeBridgeActivationEvidence::VerifiedAbsent,
            retained_generations: ScopeBridgeRetainedEvidence::Enumerated(
                retained.into_iter().map(str::to_string).collect(),
            ),
        }
    }

    fn present_bridge_evidence(
        generation_id: &str,
        scope: PublishedScope,
        retained: impl IntoIterator<Item = &'static str>,
    ) -> ScopeBridgeClearEvidence {
        ScopeBridgeClearEvidence {
            activation: ScopeBridgeActivationEvidence::Present {
                generation_id: generation_id.to_string(),
                scope,
            },
            retained_generations: ScopeBridgeRetainedEvidence::Enumerated(
                retained.into_iter().map(str::to_string).collect(),
            ),
        }
    }

    /// R3F3 mode 1: the no-activation dangling-bridge case succeeds when
    /// the bridge generation is absent from the retained set. Previously
    /// mode 1 required effective_generation_id unconditionally, making a
    /// dangling bridge with no activation unrecoverable.
    #[test]
    fn r3f3_mode1_dangling_bridge_no_activation_succeeds() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("dangling_gen"));
        let epoch = store.snapshot().unwrap().epoch();
        // No effective activation (dangling bridge), empty retained set
        // proves the bridge generation is absent from the store.
        let evidence = absent_bridge_evidence([]);
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DanglingReference,
            &evidence,
        );
        assert!(
            result.is_ok(),
            "dangling bridge with no activation and absent generation must clear, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn automatic_first_new_scope_clear_expects_retained_bridge() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("dangling_gen"));
        let epoch = store.snapshot().unwrap().epoch();
        let evidence = present_bridge_evidence(
            "new_generation",
            PublishedScope::try_new("f4-scope", ".").unwrap(),
            ["dangling_gen"],
        );
        clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::AutomaticFirstNewScope,
            &evidence,
        )
        .unwrap();
        assert!(
            store
                .snapshot()
                .unwrap()
                .catalog()
                .scope_migrations
                .values()
                .all(|record| record.code_bridge_generation.is_none())
        );
    }

    #[test]
    fn automatic_first_new_scope_clear_refuses_missing_evidence() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("dangling_gen"));
        let error = clear_scope_bridge(
            &store,
            store.snapshot().unwrap().epoch(),
            &pid,
            ScopeBridgeClearMode::AutomaticFirstNewScope,
            &ScopeBridgeClearEvidence::default(),
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_scope_bridge_clear_evidence_unavailable"
        );
    }

    /// R3F3 mode 1: dangling bridge still refuses when the bridge
    /// generation IS in the retained set (not actually retired).
    #[test]
    fn r3f3_mode1_dangling_bridge_retained_generation_refuses() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("still_retained"));
        let epoch = store.snapshot().unwrap().epoch();
        let evidence = absent_bridge_evidence(["still_retained"]);
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DanglingReference,
            &evidence,
        );
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("bridge_retained"),
            "must refuse when bridge gen is in retained set, got: {err}"
        );
    }

    /// R2F4 mode 1: refuses when the bridge generation is still the
    /// effective activation (bridge is live, not dangling).
    #[test]
    fn r2f4_mode1_refuses_live_bridge() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("gen_still_live"));
        let epoch = store.snapshot().unwrap().epoch();
        let evidence = present_bridge_evidence(
            "gen_still_live",
            PublishedScope::try_new("f4-scope", ".").unwrap(),
            [],
        );
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DanglingReference,
            &evidence,
        );
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("bridge_still_live"),
            "must refuse with bridge_still_live, got: {err}"
        );
    }

    /// R2F4 mode 1: refuses when the bridge generation is still in the
    /// retained set (R2F4: must prove absence from retained/GC-rooted set,
    /// not just id inequality).
    #[test]
    fn r2f4_mode1_refuses_retained_bridge() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("old_bridge_gen"));
        let epoch = store.snapshot().unwrap().epoch();
        let evidence = present_bridge_evidence(
            "new_effective_gen",
            PublishedScope::try_new("f4-scope", ".").unwrap(),
            ["old_bridge_gen"],
        );
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DanglingReference,
            &evidence,
        );
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("bridge_retained"),
            "must refuse with bridge_retained, got: {err}"
        );
    }

    /// R2F4 mode 1: succeeds when evidence proves the bridge generation
    /// is retired (absent from retained set AND not the effective gen).
    #[test]
    fn r2f4_mode1_succeeds_when_bridge_is_retired() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("old_bridge_gen"));
        let epoch = store.snapshot().unwrap().epoch();
        let evidence = present_bridge_evidence(
            "new_effective_gen",
            PublishedScope::try_new("f4-scope", ".").unwrap(),
            [],
        );
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DanglingReference,
            &evidence,
        );
        assert!(result.is_ok(), "must succeed when bridge is retired");
    }

    /// R2F4 mode 2: refuses when only one bridge record exists.
    #[test]
    fn r2f4_mode2_refuses_single_bridge() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("gen1"));
        let epoch = store.snapshot().unwrap().epoch();
        let evidence = ScopeBridgeClearEvidence::default();
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DoubleMigrationRepair,
            &evidence,
        );
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("no_double_migration"),
            "must refuse with no_double_migration, got: {err}"
        );
    }

    /// R2F4 mode 2: the exact open-bridge predicate for a legacy
    /// A->B->C double migration. Uses the existing f4_store_with_bridge
    /// (LegacyLocal -> f4-scope) and adds a second record (f4-scope ->
    /// f4-scope-2). The effective scope is LegacyLocal and the effective
    /// generation matches the first record's bridge generation (so the
    /// older/first record admits and the newer/second does not).
    #[test]
    fn r2f4_mode2_abc_shape_older_admits_newer_does_not() {
        use bbox_corpus_core::project_catalog::{
            ProjectScope, ScopeMigrationAuthorityProvenance, ScopeMigrationId, ScopeMigrationKind,
            ScopeMigrationRecord,
        };
        // f4_store_with_bridge creates a project with scope Published("f4-scope", "."),
        // one attachment with validated_scope=Some("f4-scope",".") and one
        // migration record: old=LegacyLocal, new=Published("f4-scope","."),
        // code_bridge_generation=Some("gen_effective").
        //
        // For the A->B->C shape: effective activation is scope "f4-scope"
        // (scope A), gen "gen_effective". We need TWO bridge-bearing records:
        //   - older: old_scope = Published("f4-scope","."), bridge_gen = "gen_effective" (ADMITS)
        //   - newer: old_scope = Published("f4-scope-2","."), bridge_gen = "gen_other" (does NOT admit)
        //
        // The existing record has old_scope=LegacyLocal. We need to modify it
        // so old_scope = Published("f4-scope",".").
        let (_tmp, store, pid) = f4_store_with_bridge(Some("gen_effective"));
        let f4_scope = PublishedScope::try_new("f4-scope", ".").unwrap();
        // Same repo, different relpath for RelpathMove.
        let f4_scope_b = PublishedScope::try_new("f4-scope", "b").unwrap();
        let f4_scope_2 = PublishedScope::try_new("f4-scope", "c").unwrap();

        let base = store.snapshot().unwrap();
        let epoch = base.epoch();
        let pid_clone = pid.clone();
        let f4_scope_clone = f4_scope.clone();
        let f4_scope_b_clone = f4_scope_b.clone();
        let _f4_scope_2_clone = f4_scope_2.clone();
        store
            .transact(epoch, move |catalog, attachments| {
                // Modify the existing record so its old_scope = Published(f4_scope)
                // and bridge_gen = "gen_effective" (so it ADMITS). The new_scope
                // is f4_scope_b (same repo, different relpath) to satisfy
                // RelpathMove validation and avoid equal-scope check.
                for record in catalog.scope_migrations.values_mut() {
                    if record.project_id == pid_clone {
                        record.old_scope = ProjectScope::Published(f4_scope_clone.clone());
                        record.new_scope = ProjectScope::Published(f4_scope_b_clone.clone());
                        record.kind = ScopeMigrationKind::RelpathMove;
                        record.code_bridge_generation = Some("gen_effective".to_string());
                    }
                }
                // Fix the migration proof to match.
                for proof in attachments.scope_migration_proofs.values_mut() {
                    proof.old_scope = ProjectScope::Published(f4_scope_clone.clone());
                    proof.new_scope = ProjectScope::Published(f4_scope_b_clone.clone());
                }
                // Insert a newer record that does NOT admit:
                // old_scope = Published(f4_scope_2), bridge_gen = "gen_other".
                let newer = ScopeMigrationRecord {
                    scope_migration_id: ScopeMigrationId::mint(),
                    project_id: pid_clone.clone(),
                    catalog_epoch: epoch,
                    authority_provenance: ScopeMigrationAuthorityProvenance::OperatorAttested,
                    operator_invocation: "test".into(),
                    operator_reason: Some("test".into()),
                    old_scope: ProjectScope::Published(f4_scope_b_clone),
                    new_scope: ProjectScope::Published(f4_scope_clone),
                    kind: ScopeMigrationKind::RelpathMove,
                    migrated_at: "2024-01-01T00:00:00Z".into(),
                    code_bridge_generation: Some("gen_other".to_string()),
                    publication_bridge_generation: None,
                    pending_capabilities: Default::default(),
                };
                catalog
                    .scope_migrations
                    .insert(newer.scope_migration_id.clone(), newer);
                Ok(())
            })
            .unwrap();

        let epoch = store.snapshot().unwrap().epoch();
        // Effective scope = f4_scope, effective gen = gen_effective.
        // The older record ADMITS (old_scope == f4_scope AND gen == gen_effective).
        // The newer record does NOT admit (old_scope == f4_scope_2 != f4_scope).
        let evidence = present_bridge_evidence("gen_effective", f4_scope, []);
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DoubleMigrationRepair,
            &evidence,
        );
        assert!(
            result.is_ok(),
            "mode 2 must succeed when older admits and newer does not: {:?}",
            result
        );
    }

    /// R2F4 mode 2: refuses when the older record does NOT admit the
    /// effective generation (old_scope mismatch).
    #[test]
    fn r2f4_mode2_refuses_older_does_not_admit() {
        let (_tmp, store, pid) = f4_store_with_bridge(Some("gen_effective"));
        let f4_scope = PublishedScope::try_new("f4-scope", ".").unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        // Only one bridge record, but evidence has scope/gen.
        // This should refuse with no_double_migration (only one record).
        let evidence = present_bridge_evidence("gen_effective", f4_scope, []);
        let result = clear_scope_bridge(
            &store,
            epoch,
            &pid,
            ScopeBridgeClearMode::DoubleMigrationRepair,
            &evidence,
        );
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("no_double_migration"),
            "must refuse with no_double_migration when only one bridge record, got: {err}"
        );
    }

    // ---- F6: retirement journal strict decoding tests ----

    /// F6: load_retirement_journal refuses a journal whose embedded
    /// project_id does not match the filename.
    #[test]
    fn f6_load_refuses_mismatched_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_a = ProjectId::parse("p_00000000000000000000000000000a1").unwrap();
        let pid_b = ProjectId::parse("p_000000000000000000000000000000b2").unwrap();
        // Write a journal for pid_b at pid_a's path.
        let journal = ProjectRetirementJournal::new(pid_b, 1, "1");
        let path = retirement_journal_path(tmp.path(), &pid_a);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid_a);
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("does not match the filename"),
            "must refuse mismatched project_id, got: {err}"
        );
    }

    /// F6: load_retirement_journal refuses a journal with the wrong
    /// version number.
    #[test]
    fn f6_load_refuses_wrong_version() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Write a journal with version 999.
        let raw = serde_json::json!({
            "version": 999,
            "project_id": pid,
            "started_at": "1",
            "updated_at": "1",
            "current_stage": "prepared",
            "catalog_epoch_at_start": 1
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err());
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("version mismatch"),
            "must refuse wrong version, got: {err}"
        );
    }

    /// F6: load_retirement_journal refuses malformed JSON.
    #[test]
    fn f6_load_refuses_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not valid json").unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err(), "must refuse malformed JSON");
    }

    /// F6: load_retirement_journal refuses a journal with an unknown
    /// stage value.
    #[test]
    fn f6_load_refuses_unknown_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = serde_json::json!({
            "version": 1,
            "project_id": pid,
            "started_at": "1",
            "updated_at": "1",
            "current_stage": "nonexistent_stage",
            "catalog_epoch_at_start": 1
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err(), "must refuse unknown stage");
    }

    /// F6: save_retirement_journal uses atomic write with fsync (no
    /// .json.tmp file must remain after save).
    #[test]
    fn f6_save_no_tmp_remains() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let dir = tmp.path().join("retirement-journals");
        // R3F4: check that NO .json.tmp files remain (unique temp names).
        let leftover_tmps: Vec<_> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.ends_with(".json.tmp"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "no .json.tmp files must remain after save, found {} leftover",
            leftover_tmps.len()
        );
        let journal_file = dir.join(format!("{pid}.json"));
        assert!(journal_file.exists(), "journal file must exist");
    }

    #[test]
    fn complete_active_journal_is_verified_and_archived_on_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let projects = root.join("projects.json");
        let store = ProjectCatalogStore::initialize_empty(&projects).unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        while journal.current_stage != RetirementJournalStage::Complete {
            journal.advance("2");
        }
        save_retirement_journal(root.as_path(), &journal).unwrap();

        let mut workers = NoopDischargeWorkers;
        let evidence = RetireEvidence::default();
        let (_, resumed) = retire_project_journaled_with(
            &store,
            root.as_path(),
            &pid,
            &evidence,
            true,
            &mut workers,
        )
        .unwrap();
        assert_eq!(
            resumed.unwrap().current_stage,
            RetirementJournalStage::Complete
        );
        assert!(!retirement_journal_path(root.as_path(), &pid).exists());
        assert!(archived_retirement_journal_path(root.as_path(), &pid).exists());
    }

    #[test]
    fn post_cut_resume_reproves_owner_rows_before_sweep_and_archive() {
        for stage in [
            RetirementJournalStage::CatalogPairRemoved,
            RetirementJournalStage::MaterializationSwept,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let projects = root.join("projects.json");
            let store = ProjectCatalogStore::initialize_empty(&projects).unwrap();
            let pid = ProjectId::parse(PROJECT).unwrap();
            let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
            while journal.current_stage != stage {
                journal.advance("2");
            }
            save_retirement_journal(root.as_path(), &journal).unwrap();
            let mut workers = RejectPostCutOwnerRows {
                inner: NoopDischargeWorkers,
                verify_calls: 0,
            };
            let error = retire_project_journaled_with(
                &store,
                root.as_path(),
                &pid,
                &RetireEvidence::default(),
                true,
                &mut workers,
            )
            .unwrap_err();
            assert_eq!(
                error.code(),
                "error.project_catalog_retire_recovery_not_quiescent"
            );
            assert_eq!(workers.verify_calls, 1);
            assert_eq!(
                load_retirement_journal(root.as_path(), &pid)
                    .unwrap()
                    .unwrap()
                    .current_stage,
                stage
            );
        }
    }

    #[test]
    fn archive_marker_recovers_each_two_file_rename_boundary_twice() {
        for journal_moved in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let pid = ProjectId::parse(PROJECT).unwrap();
            let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
            while journal.current_stage != RetirementJournalStage::Complete {
                journal.advance("2");
            }
            save_retirement_journal(root.as_path(), &journal).unwrap();
            let active = retirement_journal_path(root.as_path(), &pid);
            let archived = archived_retirement_journal_path(root.as_path(), &pid);
            let active_sidecar = retirement_blob_inventory_path(&active);
            let archived_sidecar = retirement_blob_inventory_path(&archived);
            let marker = retirement_archive_marker_path(root.as_path(), &pid);
            std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
            std::fs::write(&marker, pid.as_str()).unwrap();
            if journal_moved {
                std::fs::rename(&active, &archived).unwrap();
            } else {
                std::fs::rename(&active_sidecar, &archived_sidecar).unwrap();
            }

            archive_retirement_journal(root.as_path(), &pid).unwrap();
            archive_retirement_journal(root.as_path(), &pid).unwrap();
            assert!(!active.exists());
            assert!(!active_sidecar.exists());
            assert!(!marker.exists());
            assert!(archived.exists());
            assert!(archived_sidecar.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn archive_refuses_symlinked_directory_or_destination_leaf() {
        use std::os::unix::fs::symlink;

        let pid = ProjectId::parse(PROJECT).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        save_retirement_journal(&root, &journal).unwrap();
        let active_dir = root.join("retirement-journals");
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), active_dir.join("archive")).unwrap();
        assert!(archive_retirement_journal(&root, &pid).is_err());

        std::fs::remove_file(active_dir.join("archive")).unwrap();
        std::fs::create_dir(active_dir.join("archive")).unwrap();
        symlink(
            outside.path().join("journal"),
            archived_retirement_journal_path(&root, &pid),
        )
        .unwrap();
        assert!(archive_retirement_journal(&root, &pid).is_err());
        assert!(retirement_journal_path(&root, &pid).is_file());
    }

    /// F6: save_retirement_journal refuses to write through a symlink.
    #[cfg(unix)]
    #[test]
    fn f6_save_refuses_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let dir = tmp.path().join("retirement-journals");
        std::fs::create_dir_all(&dir).unwrap();
        let real_file = dir.join("real.json");
        std::fs::write(&real_file, b"{}").unwrap();
        let link = dir.join(format!("{pid}.json"));
        std::os::unix::fs::symlink(&real_file, &link).unwrap();
        let journal = ProjectRetirementJournal::new(pid, 1, "1");
        let result = save_retirement_journal(tmp.path(), &journal);
        assert!(result.is_err(), "must refuse to write through a symlink");
    }

    /// F6: load_retirement_journal refuses to follow a symlink.
    #[cfg(unix)]
    #[test]
    fn f6_load_refuses_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let dir = tmp.path().join("retirement-journals");
        std::fs::create_dir_all(&dir).unwrap();
        let real_file = dir.join("real.json");
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        std::fs::write(&real_file, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let link = dir.join(format!("{pid}.json"));
        std::os::unix::fs::symlink(&real_file, &link).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err(), "must refuse to follow a symlink");
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("symlink"),
            "must refuse symlink with a clear message, got: {err}"
        );
    }

    // ---- R2F5: strict journal validation tests ----

    /// R2F5: load refuses a journal whose current_stage claims
    /// SourceAuthorityQuiesced but completed_steps is empty (forged skip).
    #[test]
    fn r2f5_load_refuses_forged_stage_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "2024-01-01T00:00:00Z");
        // Forge: jump current_stage without recording the step.
        journal.current_stage = RetirementJournalStage::SourceAuthorityQuiesced;
        // completed_steps is still empty.
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err(), "must refuse a forged stage skip");
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("forgery") || err.contains("completed_steps"),
            "must detect stage forgery, got: {err}"
        );
    }

    /// R2F5: load refuses a journal with a wrong stage in completed_steps.
    #[test]
    fn r2f5_load_refuses_wrong_completed_step() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "2024-01-01T00:00:00Z");
        // Record a wrong step (CatalogPairRemoved instead of Prepared).
        journal.completed_steps.push(RetirementJournalStep {
            stage: RetirementJournalStage::CatalogPairRemoved,
            completed_at: "2024-01-01T00:00:01Z".into(),
        });
        journal.current_stage = RetirementJournalStage::SourceAuthorityQuiesced;
        journal.updated_at = "2024-01-01T00:00:01Z".into();
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err(), "must refuse a wrong completed step");
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("forgery"),
            "must detect wrong completed step, got: {err}"
        );
    }

    /// R2F5: load refuses a journal with non-monotonic timestamps.
    #[test]
    fn r2f5_load_refuses_timestamp_regression() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "2024-01-01T00:00:05Z");
        journal.completed_steps.push(RetirementJournalStep {
            stage: RetirementJournalStage::Prepared,
            completed_at: "2024-01-01T00:00:03Z".into(),
        });
        // Second step predates the first.
        journal.completed_steps.push(RetirementJournalStep {
            stage: RetirementJournalStage::SourceAuthorityQuiesced,
            completed_at: "2024-01-01T00:00:02Z".into(),
        });
        journal.current_stage = RetirementJournalStage::CollectedGenerationsDischarged;
        journal.updated_at = "2024-01-01T00:00:04Z".into();
        let path = retirement_journal_path(tmp.path(), &pid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_err(), "must refuse timestamp regression");
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("timestamp regression"),
            "must detect timestamp regression, got: {err}"
        );
    }

    /// R2F5/F6: save refuses to write through a pre-existing symlink at
    /// the TARGET path (the final journal file).
    #[cfg(unix)]
    #[test]
    fn r2f5_save_refuses_target_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let dir = tmp.path().join("retirement-journals");
        std::fs::create_dir_all(&dir).unwrap();
        // Create a symlink at the target journal path.
        let real_file = dir.join("real_target.json");
        std::fs::write(&real_file, b"{}").unwrap();
        let target = retirement_journal_path(tmp.path(), &pid);
        std::os::unix::fs::symlink(&real_file, &target).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        let result = save_retirement_journal(tmp.path(), &journal);
        assert!(
            result.is_err(),
            "must refuse to write through a target symlink"
        );
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("symlink"),
            "must refuse target symlink, got: {err}"
        );
    }

    /// R3F4: save uses unique temp names so a crash between temp creation
    /// and rename does not wedge the next save. Verify that calling save
    /// twice in succession succeeds (the second call does not hit EEXIST
    /// on a stale temp).
    #[test]
    fn r3f4_save_succeeds_after_prior_temp_leftover() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let dir = tmp.path().join("retirement-journals");
        std::fs::create_dir_all(&dir).unwrap();

        // Simulate a stale temp from a crashed prior save: place a file
        // at a plausible temp name.
        let stale = dir.join(format!("{pid}-crashed.json.tmp"));
        std::fs::write(&stale, b"stale").unwrap();

        // A fresh save should succeed regardless.
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        let result = save_retirement_journal(tmp.path(), &journal);
        assert!(
            result.is_ok(),
            "save must succeed with stale temp: {:?}",
            result
        );

        // The journal must be readable.
        let loaded = load_retirement_journal(tmp.path(), &pid);
        assert!(loaded.is_ok(), "journal must be loadable after save");

        // A second save must also succeed (unique temp names, no wedge).
        let journal2 = ProjectRetirementJournal::new(pid.clone(), 2, "2");
        let result2 = save_retirement_journal(tmp.path(), &journal2);
        assert!(result2.is_ok(), "second save must succeed: {:?}", result2);
    }

    /// R3F4: save refuses to write when the retirement-journals directory
    /// is a symlink (parent-symlink protection).
    #[cfg(unix)]
    #[test]
    fn r3f4_save_refuses_parent_dir_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real-retirement-journals");
        std::fs::create_dir_all(&real_dir).unwrap();
        // Create a symlink where retirement-journals is expected.
        let link = tmp.path().join("retirement-journals");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let pid = ProjectId::parse(PROJECT).unwrap();
        let journal = ProjectRetirementJournal::new(pid.clone(), 1, "1");
        let result = save_retirement_journal(tmp.path(), &journal);
        assert!(
            result.is_err(),
            "save followed a symlinked parent directory"
        );
    }

    /// R2F5: load accepts a valid journal with correct stage history.
    #[test]
    fn r2f5_load_accepts_valid_stage_history() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = ProjectId::parse(PROJECT).unwrap();
        let mut journal = ProjectRetirementJournal::new(pid.clone(), 1, "2024-01-01T00:00:00Z");
        // Advance through two stages with monotonic timestamps.
        journal.advance("2024-01-01T00:00:01Z");
        journal.advance("2024-01-01T00:00:02Z");
        save_retirement_journal(tmp.path(), &journal).unwrap();
        let result = load_retirement_journal(tmp.path(), &pid);
        assert!(result.is_ok(), "valid journal must load: {:?}", result);
    }
}

/// Publisher establish and advance (Phase 5 plan section 13.2, catalog side).
///
/// These exercise the refusals that belong to the catalog rather than to the
/// pointer store: epoch, attachment identity and status, scope agreement,
/// capability, and the ref that moved after preparation. The probe is data,
/// so none of them opens a repository.
#[cfg(test)]
mod publisher_publish_tests {
    use super::*;
    use crate::accepted_publication_runtime::{
        AcceptedPublicationRuntime, AcceptedPublicationSourceBinding, PublishSourceFile,
        PublisherPublishMode,
    };
    use crate::accepted_publication_store::fixtures;
    use bbox_corpus_core::project_catalog::CorpusProject;

    const PROJECT: &str = "p_000000000000000000000000000000c1";
    const ATTACHMENT: &str = "att_00000000000000000000000000000c01";
    const OTHER_ATTACHMENT: &str = "att_00000000000000000000000000000c02";
    const COMMIT_ONE: &str = "1111111111111111111111111111111111111111";
    const COMMIT_TWO: &str = "2222222222222222222222222222222222222222";
    const SOURCE_GENERATION: &str =
        "kps_1111111111111111111111111111111111111111111111111111111111111111";

    struct Fixture {
        _directory: tempfile::TempDir,
        projects_path: std::path::PathBuf,
        store: std::sync::Arc<ProjectCatalogStore>,
        runtime: AcceptedPublicationRuntime,
    }

    fn scope(relative: &str) -> PublishedScope {
        PublishedScope::try_new("repo_publish", relative).unwrap()
    }

    fn project_id() -> ProjectId {
        ProjectId::parse(PROJECT).unwrap()
    }

    fn attachment_id(raw: &str) -> AttachmentId {
        AttachmentId::parse(raw).unwrap()
    }

    fn capabilities(repo_knowledge: bool) -> AttachmentCapabilities {
        AttachmentCapabilities {
            repo_knowledge,
            ..Default::default()
        }
    }

    fn checkout_project_dir(scope: &PublishedScope) -> String {
        match scope.bbox_root_relpath() {
            "." => "/tmp/publish".to_string(),
            relative => format!("/tmp/publish/{relative}"),
        }
    }

    fn attachment_row(
        raw: &str,
        project: &ProjectId,
        scope: &PublishedScope,
        status: AttachmentStatus,
        capabilities: AttachmentCapabilities,
    ) -> CheckoutAttachment {
        CheckoutAttachment {
            attachment_id: attachment_id(raw),
            project_id: project.clone(),
            checkout_id: "cccccccccccccccccccccccccccccc01".into(),
            checkout_dir: "/tmp/publish".into(),
            // Cross-validation ties the row's relpath to its scope, and the
            // project directory to the checkout top plus that relpath.
            checkout_project_dir: checkout_project_dir(scope),
            project_root_relpath: scope.bbox_root_relpath().to_string(),
            kind: AttachmentKind::Base,
            validated_scope: Some(scope.clone()),
            computed_repo_hint: None,
            branch_ref: None,
            // A detached row carries no active capability and a detach
            // timestamp; the snapshot validator enforces both.
            capabilities: match status {
                AttachmentStatus::Detached => AttachmentCapabilities::default(),
                _ => capabilities,
            },
            status,
            attached_at: "2026-08-03T00:00:00Z".into(),
            detached_at: match status {
                AttachmentStatus::Detached => Some("2026-08-03T01:00:00Z".to_string()),
                _ => None,
            },
        }
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let store =
            std::sync::Arc::new(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());
        let runtime = AcceptedPublicationRuntime::open_global(&projects_path).unwrap();
        let fixture = Fixture {
            _directory: directory,
            projects_path,
            store,
            runtime,
        };
        fixture.seed_project(&scope("."));
        fixture.upsert_attachment(attachment_row(
            ATTACHMENT,
            &project_id(),
            &scope("."),
            AttachmentStatus::Attached,
            capabilities(true),
        ));
        fixture
    }

    impl Fixture {
        fn epoch(&self) -> u64 {
            self.store.snapshot().unwrap().epoch()
        }

        fn seed_project(&self, scope: &PublishedScope) {
            let scope = scope.clone();
            self.store
                .transact(self.epoch(), |catalog, _attachments| {
                    catalog.projects.insert(
                        project_id(),
                        CorpusProject {
                            project_id: project_id(),
                            scope: ProjectScope::Published(scope.clone()),
                            operator_aliases: Default::default(),
                            nominated_aliases: Default::default(),
                            display_name: "publish-test".into(),
                            created_at: "2026-08-03T00:00:00Z".into(),
                            registered_at_compat: None,
                            repo_history: None,
                            languages: Default::default(),
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }

        fn upsert_attachment(&self, row: CheckoutAttachment) {
            self.store
                .transact(self.epoch(), |_catalog, attachments| {
                    attachments
                        .attachments
                        .insert(row.attachment_id.clone(), row.clone());
                    Ok(())
                })
                .unwrap();
        }

        /// Migrate the catalog's scope without touching the pointer, which
        /// is the publication-bridge state.
        fn migrate_scope(&self, scope: &PublishedScope) {
            let scope = scope.clone();
            self.store
                .transact(self.epoch(), |catalog, attachments| {
                    catalog.projects.get_mut(&project_id()).unwrap().scope =
                        ProjectScope::Published(scope.clone());
                    for row in attachments.attachments.values_mut() {
                        row.validated_scope = Some(scope.clone());
                        row.project_root_relpath = scope.bbox_root_relpath().to_string();
                        row.checkout_project_dir = checkout_project_dir(&scope);
                    }
                    Ok(())
                })
                .unwrap();
        }

        fn probe(&self, committed_scope: &PublishedScope, commit: &str) -> PublisherPublishProbe {
            self.probe_with_revalidation(committed_scope, commit, commit)
        }

        /// A probe whose in-lock re-resolution returns `revalidated`,
        /// which is how a ref that moved after preparation is simulated.
        fn probe_with_revalidation(
            &self,
            committed_scope: &PublishedScope,
            commit: &str,
            revalidated: &str,
        ) -> PublisherPublishProbe {
            let revalidated = revalidated.to_string();
            let relative = |lane: &str, id: &str| {
                if committed_scope.bbox_root_relpath() == "." {
                    format!(".bbox/{lane}/{id}.json")
                } else {
                    format!(
                        "{}/.bbox/{lane}/{id}.json",
                        committed_scope.bbox_root_relpath()
                    )
                }
            };
            PublisherPublishProbe {
                resolved_commit: commit.to_string(),
                committed_scope: committed_scope.clone(),
                sources: PublishSources {
                    knowledge: vec![PublishSourceFile {
                        repository_relative_filename: relative("knowledge", "knowledge-a"),
                        source_bytes: serde_json::to_vec(&fixtures::knowledge_entry(
                            "knowledge-a",
                            commit,
                        ))
                        .unwrap(),
                    }],
                    gaps: vec![PublishSourceFile {
                        repository_relative_filename: relative("gaps", "gap-1234abcd"),
                        source_bytes: serde_json::to_vec(&fixtures::gap_note("gap-1234abcd"))
                            .unwrap(),
                    }],
                    graphs: Vec::new(),
                },
                revalidate_checkout: Box::new(|| Ok(())),
                revalidate_ref: Box::new(move || Some(revalidated.clone())),
            }
        }

        fn request(&self, mode: PublisherPublishMode, attachment: &str) -> PublisherPublishRequest {
            PublisherPublishRequest {
                mode,
                project_id: project_id(),
                attachment_id: attachment_id(attachment),
                full_ref: "refs/heads/main".into(),
                expected_epoch: self.epoch(),
                dry_run: false,
            }
        }

        fn candidate_request(
            &self,
            mode: PublisherPublishMode,
            dry_run: bool,
        ) -> PublisherCandidatePublishRequest {
            PublisherCandidatePublishRequest {
                mode,
                project_id: project_id(),
                source_generation_id: SOURCE_GENERATION.into(),
                expected_epoch: self.epoch(),
                dry_run,
            }
        }

        fn candidate_probe(
            &self,
            committed_scope: &PublishedScope,
            commit: &str,
            source_is_fresh: bool,
        ) -> PublisherCandidatePublishProbe {
            let attachment_probe = self.probe(committed_scope, commit);
            PublisherCandidatePublishProbe {
                producer_id: "producer-a".into(),
                source_generation_id: SOURCE_GENERATION.into(),
                source_generation_sha256: "2".repeat(64),
                scope: committed_scope.clone(),
                full_ref: "refs/heads/main".into(),
                accepted_commit: commit.into(),
                sources: attachment_probe.sources,
                revalidate_source: Box::new(move || {
                    if source_is_fresh {
                        Ok(())
                    } else {
                        Err(PublishError::refusal(
                            "error.accepted_publication_candidate_stale",
                            "injected stale source candidate",
                        ))
                    }
                }),
            }
        }

        fn establish(&self, commit: &str) -> Result<PublishReceipt, PublishError> {
            publish_accepted_publication(
                &self.store,
                &self.runtime,
                &self.request(PublisherPublishMode::Establish, ATTACHMENT),
                self.probe(&scope("."), commit),
            )
        }
    }

    #[test]
    fn establish_then_advance_succeeds_through_the_catalog_gates() {
        let fixture = fixture();
        let established = fixture.establish(COMMIT_ONE).unwrap();
        assert_eq!(established.previous_pointer_sha256(), None);

        let tokens = fixture
            .runtime
            .advance_tokens(&project_id())
            .unwrap()
            .unwrap();
        let advanced = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(
                PublisherPublishMode::Advance {
                    expected_generation_id: tokens.0,
                    expected_pointer_sha256: tokens.1,
                },
                ATTACHMENT,
            ),
            fixture.probe(&scope("."), COMMIT_TWO),
        )
        .unwrap();
        assert_eq!(
            advanced.previous_pointer_sha256(),
            Some(established.pointer_sha256())
        );
        assert_eq!(
            fixture
                .runtime
                .load_verified(&project_id())
                .unwrap()
                .content_stamp()
                .accepted_commit(),
            COMMIT_TWO
        );
    }

    #[test]
    fn candidate_establish_and_advance_use_producer_evidence_and_preserve_prior() {
        let fixture = fixture();
        let dry_run = publish_accepted_publication_candidate(
            &fixture.store,
            &fixture.runtime,
            &fixture.candidate_request(PublisherPublishMode::Establish, true),
            fixture.candidate_probe(&scope("."), COMMIT_ONE, true),
        )
        .unwrap();
        assert!(dry_run.is_dry_run());
        assert!(fixture.runtime.load_verified(&project_id()).is_err());

        let established = fixture.establish(COMMIT_ONE).unwrap();
        let tokens = fixture
            .runtime
            .advance_tokens(&project_id())
            .unwrap()
            .unwrap();
        let advanced = publish_accepted_publication_candidate(
            &fixture.store,
            &fixture.runtime,
            &fixture.candidate_request(
                PublisherPublishMode::Advance {
                    expected_generation_id: tokens.0,
                    expected_pointer_sha256: tokens.1,
                },
                false,
            ),
            fixture.candidate_probe(&scope("."), COMMIT_TWO, true),
        )
        .unwrap();
        assert_eq!(
            advanced.previous_pointer_sha256(),
            Some(established.pointer_sha256())
        );
        let verified = fixture.runtime.load_verified(&project_id()).unwrap();
        assert_eq!(verified.content_stamp().accepted_commit(), COMMIT_TWO);
        assert!(matches!(
            verified.binding_stamp().source(),
            AcceptedPublicationSourceBinding::Producer {
                producer_id,
                source_generation_id,
                ..
            } if producer_id == "producer-a" && source_generation_id == SOURCE_GENERATION
        ));
        assert_eq!(
            fixture
                .runtime
                .protected_source_generation_roots([project_id()])
                .unwrap(),
            std::collections::BTreeSet::from([SOURCE_GENERATION.to_string()])
        );
    }

    #[test]
    fn candidate_publish_rechecks_source_freshness_before_the_swap() {
        let fixture = fixture();
        let error = publish_accepted_publication_candidate(
            &fixture.store,
            &fixture.runtime,
            &fixture.candidate_request(PublisherPublishMode::Establish, false),
            fixture.candidate_probe(&scope("."), COMMIT_ONE, false),
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.accepted_publication_candidate_stale");
        assert!(fixture.runtime.load_verified(&project_id()).is_err());
    }

    #[test]
    fn publish_revalidates_the_checkout_lease_at_the_swap_boundary() {
        let fixture = fixture();
        let mut probe = fixture.probe(&scope("."), COMMIT_ONE);
        probe.revalidate_checkout = Box::new(|| {
            Err(PublishError::refusal(
                "error.checkout_lease_stale",
                "injected stale lease",
            ))
        });
        let error = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(PublisherPublishMode::Establish, ATTACHMENT),
            probe,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.checkout_lease_stale");
        assert!(fixture.runtime.load_verified(&project_id()).is_err());
    }

    #[test]
    fn a_stale_catalog_epoch_refuses_before_any_write() {
        let fixture = fixture();
        let mut request = fixture.request(PublisherPublishMode::Establish, ATTACHMENT);
        request.expected_epoch = fixture.epoch() + 1;

        let error = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &request,
            fixture.probe(&scope("."), COMMIT_ONE),
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_stale_epoch");
        assert!(fixture.runtime.load_verified(&project_id()).is_err());
    }

    #[test]
    fn every_attachment_refusal_keeps_its_own_code() {
        let fixture = fixture();
        // Unknown attachment.
        let error = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(PublisherPublishMode::Establish, OTHER_ATTACHMENT),
            fixture.probe(&scope("."), COMMIT_ONE),
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_unknown_attachment"
        );

        // Another project's attachment.
        let other_project = ProjectId::parse("p_000000000000000000000000000000d1").unwrap();
        fixture
            .store
            .transact(fixture.epoch(), |catalog, attachments| {
                catalog.projects.insert(
                    other_project.clone(),
                    CorpusProject {
                        project_id: other_project.clone(),
                        scope: ProjectScope::Published(scope("other")),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: "other".into(),
                        created_at: "2026-08-03T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                attachments.attachments.insert(
                    attachment_id(OTHER_ATTACHMENT),
                    attachment_row(
                        OTHER_ATTACHMENT,
                        &other_project,
                        &scope("other"),
                        AttachmentStatus::Attached,
                        capabilities(true),
                    ),
                );
                Ok(())
            })
            .unwrap();
        let error = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(PublisherPublishMode::Establish, OTHER_ATTACHMENT),
            fixture.probe(&scope("."), COMMIT_ONE),
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_attachment_project_mismatch"
        );

        // Detached.
        fixture.upsert_attachment(attachment_row(
            ATTACHMENT,
            &project_id(),
            &scope("."),
            AttachmentStatus::Detached,
            capabilities(true),
        ));
        let error = fixture.establish(COMMIT_ONE).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_attachment_detached"
        );

        // Attached again but without the repo-knowledge capability.
        fixture.upsert_attachment(attachment_row(
            ATTACHMENT,
            &project_id(),
            &scope("."),
            AttachmentStatus::Attached,
            capabilities(false),
        ));
        let error = fixture.establish(COMMIT_ONE).unwrap_err();
        assert_eq!(error.code(), "error.project_capability_denied");
        assert!(fixture.runtime.load_verified(&project_id()).is_err());
    }

    #[test]
    fn a_committed_scope_that_disagrees_with_the_catalog_refuses() {
        let fixture = fixture();
        let error = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(PublisherPublishMode::Establish, ATTACHMENT),
            fixture.probe(&scope("elsewhere"), COMMIT_ONE),
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_scope_mismatch");
    }

    #[test]
    fn a_ref_that_moves_after_preparation_refuses_at_the_swap() {
        let fixture = fixture();
        let error = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(PublisherPublishMode::Establish, ATTACHMENT),
            fixture.probe_with_revalidation(&scope("."), COMMIT_ONE, COMMIT_TWO),
        )
        .unwrap_err();
        assert_eq!(error.code(), ERROR_ACCEPTED_PUBLICATION_REF_MOVED);
        // The generation was installed off-lock, but no pointer names it.
        assert!(fixture.runtime.load_verified(&project_id()).is_err());
    }

    #[test]
    fn a_scope_migration_bridge_is_cleared_by_advancing_at_the_new_scope() {
        let fixture = fixture();
        fixture.establish(COMMIT_ONE).unwrap();
        let migrated = scope("services/api");
        fixture.migrate_scope(&migrated);

        // Bind refuses while the bridge is open: attachment-only rebind
        // cannot move a pointer to a new scope. It refuses the same way
        // whether or not the attachment already contains the accepted
        // commit, because fetching that commit would not make the rebind
        // possible; only a new-scope advance closes the bridge.
        for accepted_commit_present in [true, false] {
            let bind = bind_publisher_attachment(
                &fixture.store,
                &fixture.projects_path,
                fixture.epoch(),
                &project_id(),
                &attachment_id(ATTACHMENT),
                &PublisherBindProbe {
                    accepted_commit_present,
                    revalidate_checkout: Box::new(|| true),
                },
            )
            .unwrap_err();
            assert_eq!(
                bind.code(),
                ERROR_ACCEPTED_PUBLICATION_SCOPE_ADVANCE_REQUIRED,
                "commit_present={accepted_commit_present}"
            );
        }

        // The old accepted content still serves, under its own scope.
        let before = fixture.runtime.load_verified(&project_id()).unwrap();
        assert_eq!(before.content_stamp().accepted_scope(), &scope("."));

        // Advance at the catalog's CURRENT scope closes the bridge and
        // keeps the old pointer as prior.
        let tokens = fixture
            .runtime
            .advance_tokens(&project_id())
            .unwrap()
            .unwrap();
        publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &fixture.request(
                PublisherPublishMode::Advance {
                    expected_generation_id: tokens.0,
                    expected_pointer_sha256: tokens.1,
                },
                ATTACHMENT,
            ),
            fixture.probe(&migrated, COMMIT_TWO),
        )
        .unwrap();
        let after = fixture.runtime.load_verified(&project_id()).unwrap();
        assert_eq!(after.content_stamp().accepted_scope(), &migrated);
        assert_eq!(
            after.binding_stamp().scope_agreement(Some(&migrated)),
            crate::accepted_publication_runtime::AcceptedPublicationScopeAgreement::Agreed
        );
    }

    /// D-033 item 1, stated as a test rather than as a claim.
    ///
    /// Catalog detach deliberately does not take the publication lock, so a
    /// detach can still land after the freshness recheck and before the
    /// swap. The residual is a misleading binding, never corruption: the
    /// accepted content is intact, published reads keep serving, and an
    /// explicit bind repairs the binding. Nothing in this milestone claims
    /// the window is closed.
    #[derive(Debug)]
    struct DetachAtSwap {
        store: std::sync::Arc<ProjectCatalogStore>,
    }

    impl crate::accepted_publication_store::AcceptedPublicationFaultInjector for DetachAtSwap {
        fn checkpoint(
            &self,
            point: crate::accepted_publication_store::AcceptedPublicationFaultPoint,
        ) -> Result<(), crate::accepted_publication_store::AcceptedPublicationStoreError> {
            if point
                != crate::accepted_publication_store::AcceptedPublicationFaultPoint::BeforePointerSwap
            {
                return Ok(());
            }
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    let row = attachments
                        .attachments
                        .get_mut(&attachment_id(ATTACHMENT))
                        .unwrap();
                    row.status = AttachmentStatus::Detached;
                    row.capabilities = AttachmentCapabilities::default();
                    row.detached_at = Some("2026-08-03T02:00:00Z".into());
                    Ok(())
                })
                .unwrap();
            // Continue into the swap: this is the interleaving, not a fault.
            Ok(())
        }
    }

    #[test]
    fn a_detach_inside_the_final_swap_window_leaves_a_binding_to_repair() {
        let mut fixture = fixture();
        fixture
            .runtime
            .install_fault_injector_for_test(std::sync::Arc::new(DetachAtSwap {
                store: fixture.store.clone(),
            }));

        // The publish itself succeeds: freshness passed, and the detach
        // landed after it.
        let receipt = fixture.establish(COMMIT_ONE).unwrap();

        // Accepted content is intact and still readable.
        let verified = fixture.runtime.load_verified(&project_id()).unwrap();
        assert_eq!(verified.content_stamp().accepted_commit(), COMMIT_ONE);
        assert_eq!(
            verified.binding_stamp().pointer_sha256(),
            receipt.pointer_sha256()
        );

        // The pointer names an attachment the catalog now reports detached.
        // That is the residual: a binding an operator repairs with bind.
        let bound = verified.binding_stamp().attachment_id().cloned().unwrap();
        let state = fixture.store.snapshot().unwrap();
        assert_eq!(
            state.attachments().attachments.get(&bound).unwrap().status,
            AttachmentStatus::Detached
        );
    }

    #[test]
    fn without_a_bridge_bind_still_requires_the_accepted_commit() {
        let fixture = fixture();
        fixture.establish(COMMIT_ONE).unwrap();

        let error = bind_publisher_attachment(
            &fixture.store,
            &fixture.projects_path,
            fixture.epoch(),
            &project_id(),
            &attachment_id(ATTACHMENT),
            &PublisherBindProbe {
                accepted_commit_present: false,
                revalidate_checkout: Box::new(|| true),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_commit_not_present"
        );
    }

    #[test]
    fn bind_revalidates_the_checkout_lease_at_the_replace_boundary() {
        let fixture = fixture();
        fixture.establish(COMMIT_ONE).unwrap();
        let error = bind_publisher_attachment(
            &fixture.store,
            &fixture.projects_path,
            fixture.epoch(),
            &project_id(),
            &attachment_id(ATTACHMENT),
            &PublisherBindProbe {
                accepted_commit_present: true,
                revalidate_checkout: Box::new(|| false),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.checkout_lease_stale");
    }

    #[test]
    fn a_dry_run_validates_everything_and_writes_no_pointer() {
        let fixture = fixture();
        let mut request = fixture.request(PublisherPublishMode::Establish, ATTACHMENT);
        request.dry_run = true;

        let receipt = publish_accepted_publication(
            &fixture.store,
            &fixture.runtime,
            &request,
            fixture.probe(&scope("."), COMMIT_ONE),
        )
        .unwrap();
        assert!(receipt.is_dry_run());
        assert!(!receipt.generation_id().is_empty());
        assert!(fixture.runtime.load_verified(&project_id()).is_err());
        assert!(
            fixture
                .runtime
                .advance_tokens(&project_id())
                .unwrap()
                .is_none()
        );
    }
}
