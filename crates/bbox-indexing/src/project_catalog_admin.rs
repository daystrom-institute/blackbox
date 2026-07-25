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
