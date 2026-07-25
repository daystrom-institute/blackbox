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
                    source_store: "attachment-relocation".into(),
                    source_row_id: id.as_str().to_string(),
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

/// Daemon-probed publisher-bind evidence: the tool layer proved the new
/// attachment's object database contains the pointer's accepted commit
/// (the containment a later advance and overlay recomputation need).
#[derive(Debug, Clone)]
pub struct PublisherBindProbe {
    pub accepted_commit_present: bool,
}

#[derive(Debug, Clone)]
pub struct PublisherBindReceipt {
    pub attachment_id: AttachmentId,
}

/// Rebind the publisher attachment for one project (plan §7.7): the
/// pointer's ref, commit, scope, generation, and payloads never change
/// here; ref/commit changes are exclusively the later advance path. The
/// catalog side validates the attachment; the pointer store enforces the
/// pointer/generation agreement before and after.
pub fn bind_publisher_attachment(
    store: &ProjectCatalogStore,
    projects_path: &std::path::Path,
    project_id: &ProjectId,
    new_attachment: &AttachmentId,
    probe: &PublisherBindProbe,
) -> AdminResult<PublisherBindReceipt> {
    use crate::accepted_publication_store::{
        AcceptedPublicationLimits, AcceptedPublicationStorePaths,
        acquire_accepted_publication_lock, rebind_pointer_attachment_locked,
    };

    if !probe.accepted_commit_present {
        return Err(admin_error(
            "error.project_catalog_admin_commit_not_present",
            "the new attachment's object database does not contain the accepted \
             commit; fetch it before rebinding",
        ));
    }
    let state = store.snapshot()?;
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
    let attachment_scope = row.validated_scope.clone();

    let paths = AcceptedPublicationStorePaths::derive(projects_path)
        .map_err(|error| admin_error(error.code(), "publication paths are invalid"))?;
    let guard = acquire_accepted_publication_lock(&paths)
        .map_err(|error| admin_error(error.code(), "publication store is locked"))?;
    // Read-validate-rebind under the publication lock; no catalog lock is
    // held here (the catalog read above used a pinned snapshot).
    let Some(attachment_scope) = attachment_scope else {
        return Err(admin_error(
            "error.project_catalog_admin_scope_required",
            "a scope-less attachment cannot carry the publisher binding",
        ));
    };
    let limits = AcceptedPublicationLimits::default();
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
    Ok(PublisherBindReceipt {
        attachment_id: rebound.attachment_id,
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
                    authority: RepoHistoryAuthority::LocalProject(project_id.clone()),
                    primary_namespace: namespace,
                    compatibility_namespaces: Default::default(),
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
                            authority: RepoHistoryAuthority::Recorded(authority),
                            primary_namespace: primary,
                            compatibility_namespaces: Default::default(),
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
                source_store: "attachment-relocation".into(),
                source_row_id: attachment_id.as_str().to_string(),
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
    use bbox_corpus_core::project_catalog::{LegacyPathBindingStatus, RepoHistoryAuthority};

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
) -> AdminResult<ScopeTransitionReceipt> {
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
    let commit = store.transact(expected_epoch, move |catalog, attachments| {
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
        let (inventory, commit) = retire_project(
            &store,
            current_epoch(&store),
            &published_id,
            &evidence,
            false,
        )
        .unwrap();
        assert_eq!(inventory.blocking.get("knowledge_rows"), Some(&2));
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
        let error =
            scope_migrate_attested(&store, current_epoch(&store), &request, false).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_admin_acknowledgement_required"
        );
        let error =
            scope_migrate_attested(&store, current_epoch(&store), &request, true).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_admin_reason_required");

        request.operator_reason = Some("relocating the remote-only service root".into());
        let receipt =
            scope_migrate_attested(&store, current_epoch(&store), &request, true).unwrap();
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
        let error = scope_migrate_attested(&store, current_epoch(&store), &attached_request, true)
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
}
