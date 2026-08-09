use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use bbox_corpus_core::built_from::{BuiltFromStamp, BuiltFromTable};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_gaps::gaps::{
    BlockingLevel, GapImpact, GapKind, GapNote, GapResolution, GapStore, GapViewMetadata,
};
use bbox_gaps::overlay::{
    AcceptedPublishedGapDigests, CatalogGapOverlayPublished, GapOverlayKey,
    GapOverlayRecomputeError, GapOverlayRecomputeErrorKind, GapOverlaySnapshot, GapOverlayStatus,
    GapOverlayValue, GapTransientPreservationOutcome, PublishedGapEntry, PublishedGapSnapshot,
    WorkingGapSnapshot, load_published_snapshot_at_commit,
    recompute_catalog_overlay_result as recompute_catalog_gap_overlay_result,
    recompute_overlay_result,
};
use bbox_indexing::accepted_publication_runtime::{
    AcceptedBlockingLevelV1, AcceptedGapEntryV1, AcceptedGapImpactV1, AcceptedGapKindV1,
    AcceptedGapResolutionV1, AcceptedPublicationContentStamp, VerifiedAcceptedPublication,
};
use bbox_indexing::checkout_access::ValidatedCheckoutLease;
use bbox_knowledge::overlay::ProvisionalMode;

use super::BlackboxServer;
use super::knowledge_view::{
    CatalogOverlayAttachment, ERROR_OVERLAY_ACCEPTED_CONTENT_CHANGED,
    ERROR_OVERLAY_BASELINE_UNAVAILABLE, ERROR_OVERLAY_SNAPSHOT_STALE,
    ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE, OverlayDegradation, basename,
};

#[derive(Clone)]
pub(crate) struct PublishedGapCacheEntry {
    publisher_project_id: String,
    publisher_commit: String,
    durable_project: String,
    snapshot: PublishedGapSnapshot,
}

/// One catalog project's projected accepted gaps, valid exactly while its
/// accepted content identity is unchanged. Bounded by the catalog for the
/// same reason as the knowledge twin.
#[derive(Clone)]
pub(crate) struct CatalogPublishedGapCacheEntry {
    pub(crate) content_stamp: AcceptedPublicationContentStamp,
    pub(crate) snapshot: PublishedGapSnapshot,
}

pub(crate) struct SessionGapView {
    pub(crate) gaps: GapStore,
    pub(crate) built_from: BuiltFromTable,
    pub(crate) diagnostics: Vec<String>,
    /// Checkouts `all` omitted because they could not position themselves
    /// against accepted content, as typed rows.
    ///
    /// These are the report; the matching `diagnostics` lines are a
    /// parallel human rendering of the same facts. Empty in every other
    /// mode: `published` ignores overlay failure and `own` refuses instead
    /// of omitting, and empty on the bridge, which never builds one.
    pub(crate) degraded_overlays: Vec<OverlayDegradation>,
}

impl SessionGapView {
    pub(crate) fn built_from_for_refs<'a>(
        &self,
        refs: impl IntoIterator<Item = &'a str>,
    ) -> BuiltFromTable {
        let mut table = self.built_from.clone();
        table.retain_ids(refs);
        table
    }

    pub(crate) fn append_built_from_table(&self, output: String, table: &BuiltFromTable) -> String {
        super::built_from::append_built_from_section(output, table)
    }
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
        let projects = self.state.records_provider.records_snapshot().records;
        let prior = self
            .state
            .gap_overlays
            .read()
            .get(&checkout.published_scope, &checkout.checkout_id)
            .cloned();
        let prior_is_valid = prior
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == GapOverlayStatus::Valid);
        let mut publication_guard = None;
        let snapshot = match self
            .authorize_publisher_classified(&projects, &checkout.published_scope)
        {
            Ok(publisher) => {
                let refreshed = match self.acquire_authorized_overlay_access(&publisher, checkout) {
                    Ok((publisher_lease, checkout_lease)) => {
                        let prepared = stable_gap_overlay(
                            publisher_lease.project_root(),
                            &publisher.branch_ref,
                            &checkout_lease,
                            checkout,
                        );
                        match self
                            .state
                            .checkout_access
                            .publication_guard_for([&publisher_lease, &checkout_lease])
                        {
                            Ok(guard) => {
                                publication_guard = Some(guard);
                                prepared
                            }
                            Err(error) => Err(GapOverlayRecomputeError::transient(
                                anyhow::Error::new(error),
                            )),
                        }
                    }
                    Err(error) => Err(classify_gap_overlay_access_error(error)),
                };
                match refreshed {
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
                        match self
                            .state
                            .gap_overlays
                            .write()
                            .preserve_transient_if_latest(generation, preserved)
                        {
                            GapTransientPreservationOutcome::Preserved { .. }
                            | GapTransientPreservationOutcome::Superseded => return,
                            GapTransientPreservationOutcome::Exhausted => {
                                GapOverlaySnapshot::invalid(
                                    checkout,
                                    format!(
                                        "transient gap overlay refresh limit exceeded: {err:#}"
                                    ),
                                )
                            }
                        }
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
                match self
                    .state
                    .gap_overlays
                    .write()
                    .preserve_transient_if_latest(generation, preserved)
                {
                    GapTransientPreservationOutcome::Preserved { .. }
                    | GapTransientPreservationOutcome::Superseded => return,
                    GapTransientPreservationOutcome::Exhausted => GapOverlaySnapshot::invalid(
                        checkout,
                        format!("transient gap publisher refresh limit exceeded: {err:#}"),
                    ),
                }
            }
            Err(err) => GapOverlaySnapshot::invalid(checkout, format!("{err:#}")),
        };
        let _publication_is_held = publication_guard.as_ref();
        // Mirror the knowledge twin: a superseded publication is a normal
        // race with a newer refresh, but it must be visible, not discarded.
        if !self
            .state
            .gap_overlays
            .write()
            .publish_if_latest(generation, snapshot)
        {
            tracing::debug!(
                generation,
                "gap overlay refresh superseded by a newer requested generation"
            );
        }
    }

    /// Materialize pinned published gap records plus the selected provisional
    /// checkout layer. The returned store is detached and read-only.
    pub(crate) fn session_gap_view(
        &self,
        requested_project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<SessionGapView> {
        let session_checkout = self.authoritative_session_checkout();
        let session_workspace = self.authoritative_session_workspace_binding();
        let mode = ProvisionalMode::parse(
            provisional,
            session_checkout.is_some() || session_workspace.is_some(),
        )?;
        let projects = self.state.records_provider.records_snapshot().records;
        // Filter-class engine resolution (phase-2 §9.2): a miss keeps the
        // lenient unmanaged-scope view semantics; a hit joins the records
        // projection by identity.
        let requested_project_id = requested_project
            .and_then(|raw| self.resolve_project_filter(raw))
            .and_then(|resolution| resolution.project_id().map(str::to_owned));
        let requested_record = requested_project_id.as_ref().and_then(|project_id| {
            projects
                .iter()
                .find(|record| &record.project_id == project_id)
                .cloned()
        });
        let explicit_managed_scope = requested_record.is_some();
        let managed_paths = projects
            .iter()
            .map(|project| project.canonical_path.as_str())
            .collect::<BTreeSet<_>>();
        let mut gaps = Vec::new();
        let mut metadata = BTreeMap::<String, GapViewMetadata>::new();
        let mut built_from = BuiltFromTable::default();
        let mut diagnostics = Vec::new();
        let mut degraded_overlays = Vec::new();
        let mut has_legacy_compatibility_rows = false;
        let durable_gaps = self.state.gaps.read().all().to_vec();
        for gap in durable_gaps.iter().filter(|gap| {
            if self.path_fallback_is_cut() && gap.project.is_some() {
                return false;
            }
            !gap.project
                .as_deref()
                .is_some_and(|project| managed_paths.contains(project))
        }) {
            metadata.insert(gap.id.clone(), legacy_gap_metadata());
            gaps.push(gap.clone());
            has_legacy_compatibility_rows = true;
        }

        // Catalog published gap reads mirror the knowledge twin (plan
        // section 4.1): durable project identity to a verified accepted
        // generation, with no publisher election, publisher root, or Git.
        let catalog_published = !self.state.project_authority.is_bridge();
        if catalog_published {
            self.append_catalog_published_gaps(
                requested_project,
                requested_project_id.as_deref(),
                mode,
                session_checkout.as_deref(),
                session_workspace.as_deref(),
                &mut gaps,
                &mut metadata,
                &mut built_from,
                &mut diagnostics,
                &mut degraded_overlays,
            )?;
        }
        let selected_projects = if catalog_published {
            Vec::new()
        } else {
            requested_record
                .as_ref()
                .map(|record| vec![record.clone()])
                .unwrap_or_else(|| projects.as_ref().clone())
        };
        let mut selected_scopes = BTreeMap::<PublishedScope, ProjectRecord>::new();
        for project in selected_projects {
            match super::checkout_access::published_scope_for_project(
                &self.state.checkout_access,
                &project.project_id,
            ) {
                Ok(Some(scope)) => {
                    selected_scopes.entry(scope).or_insert(project);
                }
                Ok(None) if !self.path_fallback_is_cut() => {
                    // Inventory-bounded compatibility until the final path
                    // fallback cut: registered projects without a recorded
                    // scope keep their legacy loaded gap view.
                    for gap in durable_gaps
                        .iter()
                        .filter(|gap| gap.project.as_deref() == Some(&project.canonical_path))
                    {
                        metadata.insert(gap.id.clone(), legacy_gap_metadata());
                        gaps.push(gap.clone());
                        has_legacy_compatibility_rows = true;
                    }
                }
                Ok(None) if explicit_managed_scope => anyhow::bail!(
                    "registered project {} has no authoritative published scope",
                    project.canonical_path
                ),
                Ok(None) => diagnostics.push(format!(
                    "registered project {} has no authoritative published scope",
                    project.canonical_path
                )),
                Err(error) if explicit_managed_scope => return Err(error),
                Err(error) => diagnostics.push(format!(
                    "registered project {} scope authority failed: {error:#}",
                    project.project_id
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
            let published =
                self.cached_published_gap_snapshot(&publisher, &scope, &project.canonical_path);
            let published = match published {
                Ok(snapshot) => snapshot,
                Err(err) if explicit_managed_scope => return Err(err),
                Err(err) => {
                    diagnostics.push(format!("scope {scope:?}: {err:#}"));
                    continue;
                }
            };
            let published_ref = built_from.intern(BuiltFromStamp::Published {
                published_scope: published.published_scope.clone(),
                published_ref: published.published_ref.clone(),
                publisher_commit: published.publisher_commit.clone(),
            });
            let mut scope_gaps = published
                .gaps
                .into_iter()
                .map(|(id, entry)| {
                    metadata.insert(
                        id.clone(),
                        GapViewMetadata {
                            built_from_ref: Some(published_ref.clone()),
                            compatibility_lane: None,
                        },
                    );
                    (id, entry.gap)
                })
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
                    let cached = {
                        self.state
                            .gap_overlays
                            .read()
                            .get(&scope, &own.checkout_id)
                            .cloned()
                    };
                    let snapshot = match cached {
                        Some(snapshot) => snapshot,
                        None => {
                            self.refresh_dark_gap_overlay(own);
                            self.state
                                .gap_overlays
                                .read()
                                .get(&scope, &own.checkout_id)
                                .cloned()
                                .with_context(|| {
                                    format!(
                                        "own checkout gap overlay is missing after one bounded refresh for scope {scope:?} and checkout {}",
                                        own.checkout_id
                                    )
                                })?
                        }
                    };
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
                    let overlay_ref =
                        intern_gap_overlay_stamp(&mut built_from, &snapshot, &mut diagnostics);
                    for (id, value) in snapshot.values {
                        match value {
                            GapOverlayValue::Upsert { mut gap, .. } => {
                                gap.project = Some(project.canonical_path.clone());
                                gap.provisional_checkout_id = Some(own.checkout_id.clone());
                                metadata.insert(
                                    id.clone(),
                                    overlay_gap_metadata(overlay_ref.as_deref()),
                                );
                                scope_gaps.insert(id, *gap);
                            }
                            GapOverlayValue::Tombstone => {
                                scope_gaps.remove(&id);
                                metadata.remove(&id);
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
                        let overlay_ref =
                            intern_gap_overlay_stamp(&mut built_from, &snapshot, &mut diagnostics);
                        for (id, value) in snapshot.values {
                            match value {
                                GapOverlayValue::Upsert { mut gap, .. } => {
                                    gap.project = Some(project.canonical_path.clone());
                                    gap.provisional_checkout_id =
                                        Some(snapshot.key.checkout_id.clone());
                                    gap.id =
                                        provisional_gap_ref(&snapshot.key.checkout_id, &gap.id);
                                    metadata.insert(
                                        gap.id.clone(),
                                        overlay_gap_metadata(overlay_ref.as_deref()),
                                    );
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

        if has_legacy_compatibility_rows {
            diagnostics
                .push("legacy_compatibility gap rows have no provable built_from stamp".into());
        }
        built_from.retain_ids(
            metadata
                .values()
                .filter_map(|row| row.built_from_ref.as_deref()),
        );

        Ok(SessionGapView {
            gaps: GapStore::detached_view(gaps, metadata),
            built_from,
            diagnostics,
            degraded_overlays,
        })
    }

    /// Serve accepted published gaps for every selected catalog project.
    /// One project's missing, corrupt, or prior-generation publication is a
    /// bounded diagnostic, never a failure of the whole view.
    #[allow(clippy::too_many_arguments)] // one accumulator per view output
    fn append_catalog_published_gaps(
        &self,
        requested_selector: Option<&str>,
        requested_project_id: Option<&str>,
        mode: ProvisionalMode,
        session_checkout: Option<&ResolvedCheckoutScope>,
        session_workspace: Option<&super::knowledge_source::WorkspaceBindingGrant>,
        gaps: &mut Vec<bbox_gaps::gaps::GapNote>,
        metadata: &mut BTreeMap<String, GapViewMetadata>,
        built_from: &mut BuiltFromTable,
        diagnostics: &mut Vec<String>,
        degraded_overlays: &mut Vec<OverlayDegradation>,
    ) -> Result<()> {
        let Some(runtime) = self.state.accepted_publications.clone() else {
            diagnostics.push(
                "accepted-publication runtime is unavailable; no catalog published gaps can be \
                 served"
                    .into(),
            );
            return Ok(());
        };
        if requested_selector.is_some() && requested_project_id.is_none() {
            diagnostics.push(
                "the requested project selector did not resolve to a catalog project; every \
                 catalog project is included"
                    .into(),
            );
        }
        let targets = self.catalog_published_targets(requested_project_id)?;
        if targets.is_empty() && requested_project_id.is_some() {
            diagnostics.push("the requested project is not in the catalog".into());
            return Ok(());
        }
        for target in targets {
            let verified = match runtime.load_verified(&target.project_id) {
                Ok(verified) => verified,
                Err(error) => {
                    if self
                        .state
                        .knowledge_transport_cutover
                        .covers_project(&target.project_id)
                    {
                        self.observe_knowledge_transport_operation(
                            target.project_id.as_str(),
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::PublishedGaps,
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                        );
                    }
                    diagnostics.push(super::knowledge_view::catalog_publication_diagnostic(
                        target.project_id.as_str(),
                        &error,
                    ));
                    continue;
                }
            };
            self.observe_knowledge_transport_operation(
                target.project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::PublishedGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
            );
            diagnostics.extend(super::knowledge_view::catalog_publication_degradations(
                target.project_id.as_str(),
                &verified,
                target.catalog_scope.as_ref(),
            ));
            let published = self.cached_catalog_published_gaps(&target.project_id, &verified);
            let published_ref = built_from.intern(BuiltFromStamp::Published {
                published_scope: published.published_scope,
                published_ref: published.published_ref,
                publisher_commit: published.publisher_commit,
            });
            // The project's published rows are assembled as a map first so
            // an own-checkout tombstone can retract one before it reaches
            // the view, exactly as the bridge does.
            let mut project_gaps = published
                .gaps
                .into_iter()
                .map(|(id, entry)| {
                    metadata.insert(
                        id.clone(),
                        GapViewMetadata {
                            built_from_ref: Some(published_ref.clone()),
                            compatibility_lane: None,
                        },
                    );
                    (id, entry.gap)
                })
                .collect::<BTreeMap<_, _>>();

            match mode {
                // Published ignores overlay failure entirely: accepted
                // content is authority and needs no checkout (D-007).
                ProvisionalMode::Published => {}
                ProvisionalMode::Own => self.apply_catalog_own_gap_overlay(
                    &target.project_id,
                    &verified,
                    session_checkout,
                    session_workspace,
                    &mut project_gaps,
                    metadata,
                    built_from,
                    diagnostics,
                )?,
                ProvisionalMode::All => self.append_catalog_all_gap_overlays(
                    &target.project_id,
                    &verified,
                    gaps,
                    metadata,
                    built_from,
                    diagnostics,
                    degraded_overlays,
                )?,
            }
            gaps.extend(project_gaps.into_values());
        }
        Ok(())
    }

    /// Apply the session checkout's own provisional gap layer, or refuse.
    ///
    /// `own` is the strict mode for the same reason as its knowledge twin:
    /// the caller named one checkout, and no other attachment's ancestry
    /// may stand in for it (D-007).
    #[allow(clippy::too_many_arguments)] // one accumulator per view output
    fn apply_catalog_own_gap_overlay(
        &self,
        project_id: &ProjectId,
        verified: &VerifiedAcceptedPublication,
        session_checkout: Option<&ResolvedCheckoutScope>,
        session_workspace: Option<&super::knowledge_source::WorkspaceBindingGrant>,
        project_gaps: &mut BTreeMap<String, GapNote>,
        metadata: &mut BTreeMap<String, GapViewMetadata>,
        built_from: &mut BuiltFromTable,
        diagnostics: &mut Vec<String>,
    ) -> Result<()> {
        let transport_coverage = self
            .state
            .knowledge_transport_coverage_for_project(project_id.as_str())?
            .unwrap_or(
                bbox_indexing::knowledge_transport_cutover::KnowledgeTransportRuntimeCoverageV1::Uncovered,
            );
        if transport_coverage.transport_governed()
            && (session_workspace
                .is_some_and(|workspace| workspace.project_id != project_id.as_str())
                || session_checkout
                    .is_some_and(|checkout| checkout.project_id != project_id.as_str()))
        {
            return Ok(());
        }
        if transport_coverage.transport_governed() && !transport_coverage.current() {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            anyhow::bail!(
                "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: project {project_id} is pending knowledge transport re-cutover"
            );
        }
        if let Some(workspace) =
            session_workspace.filter(|workspace| workspace.project_id == project_id.as_str())
        {
            let pair = self
                .remote_provisional_overlays(workspace, verified)
                .map_err(|error| {
                    self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                    );
                    anyhow::anyhow!("{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: {error:#}")
                })?;
            let pair = pair.ok_or_else(|| {
                self.observe_knowledge_transport_operation(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                );
                anyhow::anyhow!(
                    "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: no live provisional generation is selected for the bound workspace"
                )
            })?;
            if pair.gaps.status != GapOverlayStatus::Valid {
                self.observe_knowledge_transport_operation(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                );
                anyhow::bail!(
                    "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: project {project_id} workspace {}: {}",
                    workspace.workspace_id,
                    pair.gaps.diagnostics.join("; ")
                );
            }
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
            );
            if !transport_coverage.transport_governed() {
                let local = self
                    .catalog_overlay_attachment(project_id, workspace.workspace_id.as_str())
                    .and_then(|attachment| {
                        attachment.map_err(super::knowledge_view::provisional_overlay_unavailable)
                    })
                    .and_then(|attachment| {
                        self.refresh_catalog_gap_overlay(verified, &attachment)
                            .map_err(super::knowledge_view::provisional_overlay_unavailable)
                    });
                match local {
                    Ok(local) if local.status == GapOverlayStatus::Valid => {
                        self.observe_knowledge_transport_operation(
                            project_id.as_str(),
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
                        );
                        self.observe_knowledge_transport_shadow(
                            project_id.as_str(),
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                            Some(workspace.workspace_id.as_str()),
                            &local.snapshot_id,
                            &pair.gaps.snapshot_id,
                        );
                    }
                    Ok(_) | Err(_) => self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                    ),
                }
            }
            diagnostics.extend(pair.gaps.diagnostics.iter().map(|diagnostic| {
                format!(
                    "project {project_id} gap workspace {}: {diagnostic}",
                    workspace.workspace_id
                )
            }));
            let overlay_ref = intern_gap_overlay_stamp(built_from, &pair.gaps, diagnostics);
            for (id, value) in pair.gaps.values {
                match value {
                    GapOverlayValue::Upsert { mut gap, .. } => {
                        stamp_catalog_gap(&mut gap, project_id, workspace.workspace_id.as_str());
                        metadata.insert(id.clone(), overlay_gap_metadata(overlay_ref.as_deref()));
                        project_gaps.insert(id, *gap);
                    }
                    GapOverlayValue::Tombstone => {
                        project_gaps.remove(&id);
                        metadata.remove(&id);
                    }
                }
            }
            return Ok(());
        }
        if transport_coverage.transport_governed() {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            anyhow::bail!(
                "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: project {project_id} requires a live bound remote workspace"
            );
        }
        let Some(own) = session_checkout.filter(|own| own.project_id == project_id.as_str()) else {
            return Ok(());
        };
        let attachment = self
            .catalog_overlay_attachment(project_id, &own.checkout_id)
            .context("selecting the attachment carrying the session checkout")?
            .map_err(|degradation| {
                anyhow::anyhow!(
                    "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: {}",
                    degradation.diagnostic_line()
                )
            })?;
        let snapshot = self
            .refresh_catalog_gap_overlay(verified, &attachment)
            .map_err(|degradation| {
                anyhow::anyhow!(
                    "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: {}",
                    degradation.diagnostic_line()
                )
            })?;
        if snapshot.status != GapOverlayStatus::Valid {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            anyhow::bail!(
                "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: project {project_id} checkout {}: {}",
                own.checkout_id,
                snapshot.diagnostics.join("; ")
            );
        }
        self.observe_knowledge_transport_operation(
            project_id.as_str(),
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
        );
        if let Some(workspace) = session_workspace.filter(|workspace| {
            workspace.project_id == project_id.as_str()
                && workspace.workspace_id.as_str() == own.checkout_id
        }) {
            match self.remote_provisional_overlays(workspace, verified) {
                Ok(Some(remote)) => self.observe_knowledge_transport_shadow(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                    Some(workspace.workspace_id.as_str()),
                    &snapshot.snapshot_id,
                    &remote.gaps.snapshot_id,
                ),
                Ok(None) | Err(_) => self.observe_knowledge_transport_operation(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnGaps,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                ),
            }
        }
        diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
            format!(
                "project {project_id} gap checkout {}: {diagnostic}",
                snapshot.key.checkout_id
            )
        }));
        let overlay_ref = intern_gap_overlay_stamp(built_from, &snapshot, diagnostics);
        for (id, value) in snapshot.values {
            match value {
                GapOverlayValue::Upsert { mut gap, .. } => {
                    stamp_catalog_gap(&mut gap, project_id, &snapshot.key.checkout_id);
                    metadata.insert(id.clone(), overlay_gap_metadata(overlay_ref.as_deref()));
                    project_gaps.insert(id, *gap);
                }
                GapOverlayValue::Tombstone => {
                    project_gaps.remove(&id);
                    metadata.remove(&id);
                }
            }
        }
        Ok(())
    }

    /// Add every peer checkout's provisional gaps, omitting only the peers
    /// that failed and reporting each one.
    #[allow(clippy::too_many_arguments)] // one accumulator per view output
    fn append_catalog_all_gap_overlays(
        &self,
        project_id: &ProjectId,
        verified: &VerifiedAcceptedPublication,
        gaps: &mut Vec<GapNote>,
        metadata: &mut BTreeMap<String, GapViewMetadata>,
        built_from: &mut BuiltFromTable,
        diagnostics: &mut Vec<String>,
        degraded_overlays: &mut Vec<OverlayDegradation>,
    ) -> Result<()> {
        let transport_coverage = self
            .state
            .knowledge_transport_coverage_for_project(project_id.as_str())?
            .unwrap_or(
                bbox_indexing::knowledge_transport_cutover::KnowledgeTransportRuntimeCoverageV1::Uncovered,
            );
        if transport_coverage.transport_governed() && !transport_coverage.current() {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            diagnostics.push(format!(
                "project {project_id} provisional gap peers are unavailable pending knowledge transport re-cutover"
            ));
            return Ok(());
        }
        let attachments = if transport_coverage.transport_governed() {
            Vec::new()
        } else {
            self.catalog_active_overlay_attachments(project_id)?
        };
        let mut seen_checkouts = attachments
            .iter()
            .map(|attachment| attachment.checkout_id.clone())
            .collect::<BTreeSet<_>>();
        for attachment in attachments {
            let degraded = match self.refresh_catalog_gap_overlay(verified, &attachment) {
                Ok(snapshot) if snapshot.status == GapOverlayStatus::Valid => {
                    self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
                    );
                    if let Ok(workspace_id) =
                        bro_core::WorkspaceId::parse(attachment.checkout_id.clone())
                    {
                        match self.remote_provisional_overlays_for_workspace(
                            project_id.as_str(),
                            verified.content_stamp().accepted_scope(),
                            &workspace_id,
                            verified,
                        ) {
                            Ok(Some(remote)) => self.observe_knowledge_transport_shadow(
                                project_id.as_str(),
                                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                                Some(workspace_id.as_str()),
                                &snapshot.snapshot_id,
                                &remote.gaps.snapshot_id,
                            ),
                            Ok(None) | Err(_) => self.observe_knowledge_transport_operation(
                                project_id.as_str(),
                                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                            ),
                        }
                    }
                    add_catalog_gap_overlay_rows(
                        project_id,
                        &snapshot,
                        gaps,
                        metadata,
                        built_from,
                        diagnostics,
                    );
                    continue;
                }
                Ok(snapshot) => OverlayDegradation {
                    project_id: project_id.as_str().to_string(),
                    checkout_id: attachment.checkout_id.clone(),
                    attachment_id: Some(attachment.attachment_id.clone()),
                    code: ERROR_OVERLAY_SNAPSHOT_STALE,
                    detail: snapshot.diagnostics.join("; "),
                    transient: false,
                },
                Err(degradation) => degradation,
            };
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            // The peer is omitted, never faked. The typed row is the
            // report; the diagnostic line renders the same facts for the
            // text surface.
            diagnostics.push(degraded.diagnostic_line());
            degraded_overlays.push(degraded);
        }
        let remote_workspaces = match self
            .state
            .knowledge_sources
            .store()
            .selected_provisional_workspace_ids_for_project(project_id.as_str())
        {
            Ok(workspaces) => workspaces,
            Err(error) => {
                self.observe_knowledge_transport_operation(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                );
                diagnostics.push(format!(
                    "project {project_id} remote provisional gap workspace inventory is unavailable: {error:#}"
                ));
                return Ok(());
            }
        };
        for workspace_id in remote_workspaces
            .into_iter()
            .filter(|workspace| seen_checkouts.insert(workspace.as_str().to_string()))
        {
            let degraded = match self.remote_provisional_overlays_for_workspace(
                project_id.as_str(),
                verified.content_stamp().accepted_scope(),
                &workspace_id,
                verified,
            ) {
                Ok(Some(pair)) if pair.gaps.status == GapOverlayStatus::Valid => {
                    self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
                    );
                    add_catalog_gap_overlay_rows(
                        project_id,
                        &pair.gaps,
                        gaps,
                        metadata,
                        built_from,
                        diagnostics,
                    );
                    continue;
                }
                Ok(Some(pair)) => OverlayDegradation {
                    project_id: project_id.as_str().to_string(),
                    checkout_id: workspace_id.as_str().to_string(),
                    attachment_id: None,
                    code: ERROR_OVERLAY_SNAPSHOT_STALE,
                    detail: pair.gaps.diagnostics.join("; "),
                    transient: false,
                },
                Ok(None) => OverlayDegradation {
                    project_id: project_id.as_str().to_string(),
                    checkout_id: workspace_id.as_str().to_string(),
                    attachment_id: None,
                    code: ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE,
                    detail: "no live provisional generation is selected".into(),
                    transient: false,
                },
                Err(error) => OverlayDegradation {
                    project_id: project_id.as_str().to_string(),
                    checkout_id: workspace_id.as_str().to_string(),
                    attachment_id: None,
                    code: ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE,
                    detail: format!("{error:#}"),
                    transient: false,
                },
            };
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllGaps,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            diagnostics.push(degraded.diagnostic_line());
            degraded_overlays.push(degraded);
        }
        Ok(())
    }

    /// Recompute one catalog checkout's provisional gap overlay against
    /// verified accepted content.
    ///
    /// The knowledge twin in `knowledge_view.rs` carries the reasoning for
    /// the shape; the two lanes differ only in which committed directory
    /// they diff and which store they publish into.
    pub(crate) fn refresh_catalog_gap_overlay(
        &self,
        verified: &VerifiedAcceptedPublication,
        attachment: &CatalogOverlayAttachment,
    ) -> std::result::Result<GapOverlaySnapshot, OverlayDegradation> {
        let _refresh = self.state.gap_overlay_refresh.lock();
        let content_stamp = verified.content_stamp();
        let scope = content_stamp.accepted_scope().clone();
        let key = GapOverlayKey {
            published_scope: scope.clone(),
            checkout_id: attachment.checkout_id.clone(),
        };

        let generation = self.state.gap_overlays.write().begin_refresh(key.clone());
        let prior = self
            .state
            .gap_overlays
            .read()
            .get(&scope, &attachment.checkout_id)
            .cloned();
        let prior_is_valid = prior
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == GapOverlayStatus::Valid);

        let failure = match self.compute_catalog_gap_overlay(verified, attachment, &scope, || {}) {
            Ok(snapshot) => {
                self.state
                    .gap_overlays
                    .write()
                    .publish_if_latest(generation, snapshot.clone());
                return Ok(snapshot);
            }
            Err(failure) => failure,
        };

        // Bounded preservation for transient failures only. A structural
        // failure carries `transient = false` by construction, so a missing
        // accepted commit or an absent merge base can never be masked by a
        // stale valid snapshot (plan section 4.12).
        if failure.transient && prior_is_valid {
            let mut preserved = prior.expect("prior valid snapshot");
            preserved.diagnostics = vec![failure.diagnostic_line()];
            match self
                .state
                .gap_overlays
                .write()
                .preserve_transient_if_latest(generation, preserved.clone())
            {
                GapTransientPreservationOutcome::Preserved { .. }
                | GapTransientPreservationOutcome::Superseded => return Ok(preserved),
                GapTransientPreservationOutcome::Exhausted => {}
            }
        }
        self.state.gap_overlays.write().publish_if_latest(
            generation,
            invalid_gap_overlay(&key, failure.diagnostic_line()),
        );
        Err(failure)
    }

    /// One checkout positioned against accepted gap content, with the lease
    /// and the accepted identity both proved after the capture.
    ///
    /// `after_capture` runs inside the capture window. Production passes a
    /// no-op; it is the test seam for movement at the exact point these
    /// proofs exist to catch.
    fn compute_catalog_gap_overlay(
        &self,
        verified: &VerifiedAcceptedPublication,
        attachment: &CatalogOverlayAttachment,
        scope: &PublishedScope,
        after_capture: impl FnMut(),
    ) -> std::result::Result<GapOverlaySnapshot, OverlayDegradation> {
        let content_stamp = verified.content_stamp();
        let project_id = content_stamp.project_id();
        let published = accepted_gap_digests(verified);
        let lease = self.acquire_catalog_overlay_lease(project_id, attachment, scope)?;
        let snapshot = stable_catalog_gap_overlay(
            CatalogGapOverlayPublished {
                published_scope: scope,
                checkout_id: &attachment.checkout_id,
                full_ref: content_stamp.full_ref(),
                accepted_commit: content_stamp.accepted_commit(),
                accepted_generation: content_stamp.generation_id(),
                published: &published,
            },
            &lease,
            after_capture,
        )
        .map_err(|error| gap_recompute_degradation(project_id, attachment, &error))?;
        self.state
            .checkout_access
            .revalidate(&lease)
            .map_err(|error| {
                OverlayDegradation::from_checkout_access(project_id, Some(attachment), &error)
            })?;
        if !self.catalog_accepted_content_unchanged(content_stamp) {
            return Err(OverlayDegradation {
                project_id: project_id.as_str().to_string(),
                checkout_id: attachment.checkout_id.clone(),
                attachment_id: Some(attachment.attachment_id.clone()),
                code: ERROR_OVERLAY_ACCEPTED_CONTENT_CHANGED,
                detail: "accepted content advanced while the overlay was being computed".into(),
                transient: false,
            });
        }
        Ok(snapshot)
    }

    /// Project accepted gap records once per accepted content identity; the
    /// content stamp is the validity token.
    fn cached_catalog_published_gaps(
        &self,
        project_id: &ProjectId,
        verified: &VerifiedAcceptedPublication,
    ) -> PublishedGapSnapshot {
        let content_stamp = verified.content_stamp();
        let cached = self
            .state
            .catalog_gap_published_cache
            .read()
            .get(project_id)
            .filter(|entry| &entry.content_stamp == content_stamp)
            .map(|entry| entry.snapshot.clone());
        if let Some(cached) = cached {
            return cached;
        }
        let snapshot = published_gaps_from_accepted(verified);
        self.state.catalog_gap_published_cache.write().insert(
            project_id.clone(),
            CatalogPublishedGapCacheEntry {
                content_stamp: content_stamp.clone(),
                snapshot: snapshot.clone(),
            },
        );
        snapshot
    }

    fn cached_published_gap_snapshot(
        &self,
        publisher: &super::knowledge_lifecycle::AuthorizedPublisher,
        scope: &PublishedScope,
        durable_project: &str,
    ) -> Result<PublishedGapSnapshot> {
        let cached = self
            .state
            .gap_published_cache
            .read()
            .get(scope)
            .filter(|entry| {
                entry.publisher_project_id == publisher.project_id
                    && entry.snapshot.published_ref == publisher.branch_ref
                    && entry.publisher_commit == publisher.commit
                    && entry.durable_project == durable_project
            })
            .cloned();
        if let Some(cached) = cached {
            return Ok(cached.snapshot.clone());
        }

        let snapshot = self.with_authorized_publisher_root(publisher, |root| {
            load_published_snapshot_at_commit(
                root,
                &publisher.branch_ref,
                &publisher.commit,
                scope,
                durable_project,
            )
        })?;
        self.state.gap_published_cache.write().insert(
            scope.clone(),
            PublishedGapCacheEntry {
                publisher_project_id: publisher.project_id.clone(),
                publisher_commit: publisher.commit.clone(),
                durable_project: durable_project.to_string(),
                snapshot: snapshot.clone(),
            },
        );
        Ok(snapshot)
    }
}

/// Project one verified accepted generation into the published gap snapshot
/// the view layer already consumes. The manifest supplies the content hash
/// of the exact committed bytes, matching what a publisher-root read of the
/// same commit would produce.
fn published_gaps_from_accepted(verified: &VerifiedAcceptedPublication) -> PublishedGapSnapshot {
    let content_stamp = verified.content_stamp();
    let mut gaps = BTreeMap::new();
    for manifest in verified.gap_manifest().values() {
        // Generation validation makes the manifest and the normalized
        // records a bijection, so a miss here is unreachable.
        let Some(record) = verified.gap_records().get(&manifest.record_id) else {
            continue;
        };
        let gap = gap_note_from_accepted(record, content_stamp.project_id());
        gaps.insert(
            gap.id.clone(),
            PublishedGapEntry {
                gap,
                content_hash: manifest.source_content_sha256.as_str().to_string(),
            },
        );
    }
    PublishedGapSnapshot {
        published_scope: content_stamp.accepted_scope().clone(),
        published_ref: content_stamp.full_ref().to_string(),
        publisher_commit: content_stamp.accepted_commit().to_string(),
        gaps,
    }
}

/// Rebuild the domain gap from its accepted record. The host-local carrier
/// fields stay absent: `project` is a checkout path, `write_dir` is a
/// transient write carrier, and a provisional checkout id belongs to an
/// overlay row, not to accepted published truth. Identity travels in
/// `project_id`.
fn gap_note_from_accepted(record: &AcceptedGapEntryV1, project_id: &ProjectId) -> GapNote {
    GapNote {
        id: record.id.as_str().to_string(),
        title: record.title.clone(),
        gap_kind: match record.gap_kind {
            AcceptedGapKindV1::PacketAst => GapKind::PacketAst,
            AcceptedGapKindV1::Tooling => GapKind::Tooling,
            AcceptedGapKindV1::Agent => GapKind::Agent,
            AcceptedGapKindV1::Workflow => GapKind::Workflow,
            AcceptedGapKindV1::RefactorPrimitive => GapKind::RefactorPrimitive,
            AcceptedGapKindV1::McpSurface => GapKind::McpSurface,
            AcceptedGapKindV1::Ontology => GapKind::Ontology,
            AcceptedGapKindV1::EvalCoverage => GapKind::EvalCoverage,
            AcceptedGapKindV1::DocsRunbook => GapKind::DocsRunbook,
        },
        domain: record.domain.clone(),
        wanted_capability: record.wanted_capability.clone(),
        missing_primitive: record.missing_primitive.clone(),
        fallback_used: record.fallback_used.clone(),
        evidence: record.evidence.clone(),
        impact: match record.impact {
            AcceptedGapImpactV1::Low => GapImpact::Low,
            AcceptedGapImpactV1::Medium => GapImpact::Medium,
            AcceptedGapImpactV1::High => GapImpact::High,
            AcceptedGapImpactV1::Critical => GapImpact::Critical,
        },
        blocking_level: match record.blocking_level {
            AcceptedBlockingLevelV1::None => BlockingLevel::None,
            AcceptedBlockingLevelV1::WorkaroundAvailable => BlockingLevel::WorkaroundAvailable,
            AcceptedBlockingLevelV1::BlocksTask => BlockingLevel::BlocksTask,
            AcceptedBlockingLevelV1::BlocksClassOfWork => BlockingLevel::BlocksClassOfWork,
        },
        dedupe_key: record.dedupe_key.clone(),
        suggested_owner: record.suggested_owner.clone(),
        notes: record.notes.clone(),
        supersedes: record.supersedes.clone(),
        superseded_by: record.superseded_by.clone(),
        resolution: match record.resolution {
            AcceptedGapResolutionV1::Unresolved => GapResolution::Unresolved,
            AcceptedGapResolutionV1::Acknowledged => GapResolution::Acknowledged,
            AcceptedGapResolutionV1::Addressed => GapResolution::Addressed,
        },
        project: None,
        project_id: Some(project_id.as_str().to_string()),
        write_dir: None,
        provisional_checkout_id: None,
        task_id: record.task_id.clone(),
        session_id: record.session_id.clone(),
        provider: record.provider.clone(),
        bro: record.bro.clone(),
        thread_id: record.thread_id.clone(),
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        resolved_at: record.resolved_at.clone(),
        resolution_note: record.resolution_note.clone(),
    }
}

fn provisional_gap_ref(checkout_id: &str, gap_id: &str) -> String {
    format!("provisional_gap:{checkout_id}:{gap_id}")
}

fn legacy_gap_metadata() -> GapViewMetadata {
    GapViewMetadata {
        built_from_ref: None,
        compatibility_lane: Some("legacy_compatibility".into()),
    }
}

fn overlay_gap_metadata(built_from_ref: Option<&str>) -> GapViewMetadata {
    GapViewMetadata {
        built_from_ref: built_from_ref.map(str::to_owned),
        compatibility_lane: built_from_ref
            .is_none()
            .then(|| "legacy_compatibility".into()),
    }
}

fn intern_gap_overlay_stamp(
    table: &mut BuiltFromTable,
    snapshot: &GapOverlaySnapshot,
    diagnostics: &mut Vec<String>,
) -> Option<String> {
    let Some(stamp) = snapshot.stamp.as_ref() else {
        diagnostics.push(format!(
            "checkout {} gap overlay has no provable built_from stamp; rows remain in legacy_compatibility",
            snapshot.key.checkout_id
        ));
        return None;
    };
    Some(table.intern(BuiltFromStamp::CheckoutOverlay {
        published_scope: stamp.published_scope.clone(),
        checkout_id: stamp.checkout_id.clone(),
        publisher_commit: stamp.publisher_commit.clone(),
        checkout_head: stamp.checkout_head.clone(),
        merge_base: stamp.merge_base.clone(),
        working_fingerprint: stamp.working_fingerprint.clone(),
    }))
}

// ── Catalog gap overlay baseline path (plan section 8, P5-D) ─────────────

/// Stamp one catalog overlay gap row.
///
/// `project` is a checkout path and a catalog row has none, so identity
/// travels in `project_id`, matching the published rows beside it. The
/// provisional checkout id stays, because that is what makes the row a
/// checkout's claim rather than published truth.
fn stamp_catalog_gap(gap: &mut GapNote, project_id: &ProjectId, checkout_id: &str) {
    gap.project = None;
    gap.project_id = Some(project_id.as_str().to_string());
    gap.provisional_checkout_id = Some(checkout_id.to_string());
}

/// Merge one valid peer snapshot's provisional gaps into the view.
fn add_catalog_gap_overlay_rows(
    project_id: &ProjectId,
    snapshot: &GapOverlaySnapshot,
    gaps: &mut Vec<GapNote>,
    metadata: &mut BTreeMap<String, GapViewMetadata>,
    built_from: &mut BuiltFromTable,
    diagnostics: &mut Vec<String>,
) {
    diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
        format!(
            "project {project_id} gap checkout {}: {diagnostic}",
            snapshot.key.checkout_id
        )
    }));
    let overlay_ref = intern_gap_overlay_stamp(built_from, snapshot, diagnostics);
    for (id, value) in &snapshot.values {
        match value {
            GapOverlayValue::Upsert { gap, .. } => {
                let mut gap = (**gap).clone();
                stamp_catalog_gap(&mut gap, project_id, &snapshot.key.checkout_id);
                // A peer's row is a distinct entity, never a replacement
                // for the published one it varies from.
                gap.id = provisional_gap_ref(&snapshot.key.checkout_id, &gap.id);
                metadata.insert(gap.id.clone(), overlay_gap_metadata(overlay_ref.as_deref()));
                gaps.push(gap);
            }
            // A peer's tombstone is a diagnostic, never a deletion: one
            // peer may not retract another's published rows.
            GapOverlayValue::Tombstone => diagnostics.push(format!(
                "checkout {} tombstones gap {id}",
                snapshot.key.checkout_id
            )),
        }
    }
}

/// An empty invalid snapshot for one gap overlay key. The bridge builds
/// this from a `ResolvedCheckoutScope`; a catalog refresh has no such
/// compatibility carrier and builds it from the key it already reserved.
fn invalid_gap_overlay(key: &GapOverlayKey, diagnostic: String) -> GapOverlaySnapshot {
    GapOverlaySnapshot {
        snapshot_id: String::new(),
        key: key.clone(),
        stamp: None,
        status: GapOverlayStatus::Invalid,
        values: BTreeMap::new(),
        diagnostics: vec![diagnostic],
    }
}

fn gap_recompute_degradation(
    project_id: &ProjectId,
    attachment: &CatalogOverlayAttachment,
    error: &GapOverlayRecomputeError,
) -> OverlayDegradation {
    let (code, detail, transient) = match error.kind {
        GapOverlayRecomputeErrorKind::BaselineUnavailable => (
            ERROR_OVERLAY_BASELINE_UNAVAILABLE,
            "the checkout does not contain the accepted commit or shares no merge base with it",
            false,
        ),
        GapOverlayRecomputeErrorKind::InvalidContent => (
            ERROR_OVERLAY_SNAPSHOT_STALE,
            "the checkout's gap files are not valid published content",
            false,
        ),
        GapOverlayRecomputeErrorKind::Transient => (
            ERROR_OVERLAY_SNAPSHOT_STALE,
            "the checkout did not settle into a stable overlay snapshot",
            true,
        ),
    };
    tracing::debug!(
        project_id = %project_id,
        checkout_id = %attachment.checkout_id,
        code,
        error = %error,
        "catalog gap overlay recompute failed"
    );
    OverlayDegradation {
        project_id: project_id.as_str().to_string(),
        checkout_id: attachment.checkout_id.clone(),
        attachment_id: Some(attachment.attachment_id.clone()),
        code,
        detail: detail.to_string(),
        transient,
    }
}

/// Project the accepted gap manifest into the identity the diff asks for.
/// Manifest keys are repository-relative; the diff compares basenames
/// inside one published scope's gap directory.
pub(crate) fn accepted_gap_digests(
    verified: &VerifiedAcceptedPublication,
) -> AcceptedPublishedGapDigests {
    AcceptedPublishedGapDigests(
        verified
            .gap_manifest()
            .iter()
            .filter_map(|(filename, manifest)| {
                Some((
                    basename(filename.as_str())?,
                    manifest.source_content_sha256.as_str().to_string(),
                ))
            })
            .collect(),
    )
}

/// Capture one gap overlay the checkout agrees with twice in a row.
///
/// Head, merge base, and working fingerprint all ride the snapshot id, so
/// one comparison covers every kind of movement. `after_capture` runs
/// between the reads; production passes a no-op.
fn stable_catalog_gap_overlay(
    published: CatalogGapOverlayPublished<'_>,
    lease: &ValidatedCheckoutLease,
    mut after_capture: impl FnMut(),
) -> std::result::Result<GapOverlaySnapshot, GapOverlayRecomputeError> {
    let pending = || {
        lease
            .checkout_relative_regular_file_exists(
                ".bbox/local/knowledge-transactions/pending.json",
            )
            .map_err(anyhow::Error::new)
            .map_err(GapOverlayRecomputeError::transient)
    };
    let working = || {
        let files = lease
            .read_relative_json_directory(".bbox/gaps")
            .map_err(anyhow::Error::new)
            .map_err(GapOverlayRecomputeError::transient)?;
        WorkingGapSnapshot::new(files).map_err(GapOverlayRecomputeError::transient)
    };
    if pending()? {
        return Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
            "checkout transaction is pending; catalog gap overlay refresh deferred"
        )));
    }
    let first_working = working()?;
    let mut candidate =
        recompute_catalog_gap_overlay_result(published, lease.checkout_root(), &first_working)?;
    for _ in 0..2 {
        after_capture();
        if pending()? {
            return Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
                "checkout transaction began during catalog gap overlay refresh"
            )));
        }
        let next_working = working()?;
        let next =
            recompute_catalog_gap_overlay_result(published, lease.checkout_root(), &next_working)?;
        if same_gap_snapshot(&candidate, &next) && !pending()? {
            return Ok(next);
        }
        candidate = next;
    }
    Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
        "checkout state changed repeatedly during catalog gap overlay refresh"
    )))
}

fn stable_gap_overlay(
    publisher_root: &Path,
    published_ref: &str,
    checkout_lease: &bbox_indexing::checkout_access::ValidatedCheckoutLease,
    checkout: &ResolvedCheckoutScope,
) -> std::result::Result<GapOverlaySnapshot, GapOverlayRecomputeError> {
    let pending = || {
        checkout_lease
            .checkout_relative_regular_file_exists(
                ".bbox/local/knowledge-transactions/pending.json",
            )
            .map_err(anyhow::Error::new)
            .map_err(GapOverlayRecomputeError::transient)
    };
    let working = || {
        let files = checkout_lease
            .read_relative_json_directory(".bbox/gaps")
            .map_err(anyhow::Error::new)
            .map_err(GapOverlayRecomputeError::transient)?;
        WorkingGapSnapshot::new(files).map_err(GapOverlayRecomputeError::transient)
    };
    if pending()? {
        return Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
            "checkout transaction is pending; provisional gap refresh deferred"
        )));
    }
    let first_working = working()?;
    let mut candidate = recompute_overlay_result(
        publisher_root,
        published_ref,
        checkout_lease.checkout_root(),
        &first_working,
        checkout,
    )?;
    for _ in 0..2 {
        if pending()? {
            return Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
                "checkout transaction began during provisional gap refresh"
            )));
        }
        let next_working = working()?;
        let next = recompute_overlay_result(
            publisher_root,
            published_ref,
            checkout_lease.checkout_root(),
            &next_working,
            checkout,
        )?;
        if same_gap_snapshot(&candidate, &next) && !pending()? {
            return Ok(next);
        }
        candidate = next;
    }
    Err(GapOverlayRecomputeError::transient(anyhow::anyhow!(
        "checkout state changed repeatedly during provisional gap refresh"
    )))
}

fn classify_gap_overlay_access_error(error: anyhow::Error) -> GapOverlayRecomputeError {
    if error
        .downcast_ref::<bbox_indexing::checkout_access::CheckoutAccessError>()
        .is_some_and(|access| {
            super::knowledge_lifecycle::checkout_access_error_is_definitively_stale(access.code)
        })
    {
        return GapOverlayRecomputeError::invalid_content(error);
    }
    match error.downcast::<GapOverlayRecomputeError>() {
        Ok(error) => error,
        Err(error) => GapOverlayRecomputeError::transient(error),
    }
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
    fn gap_overlay_access_classification_matches_reconciliation_staleness() {
        use bbox_indexing::checkout_access::{CheckoutAccessError, CheckoutAccessErrorCode};

        let stale =
            classify_gap_overlay_access_error(anyhow::Error::new(CheckoutAccessError::new(
                CheckoutAccessErrorCode::CheckoutIdentityMismatch,
                "stale checkout identity",
            )));
        assert_eq!(stale.kind, GapOverlayRecomputeErrorKind::InvalidContent);

        let transient =
            classify_gap_overlay_access_error(anyhow::Error::new(CheckoutAccessError::new(
                CheckoutAccessErrorCode::LifecycleBusy,
                "temporary lifecycle lock",
            )));
        assert_eq!(transient.kind, GapOverlayRecomputeErrorKind::Transient);
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
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&repo)
            .expect("register");
        let server = BlackboxServer::new(state.clone());

        let first = server
            .session_gap_view(Some(repo.to_str().expect("utf8 repo")), Some("published"))
            .expect("first gap view");
        assert_eq!(first.gaps.all().len(), 1);
        assert_eq!(first.built_from.len(), 1);
        let published_ref = first
            .gaps
            .view_metadata("gap-12345678")
            .and_then(|metadata| metadata.built_from_ref.as_deref())
            .expect("published gap stamp ref");
        assert!(matches!(
            first.built_from.get(published_ref),
            Some(BuiltFromStamp::Published {
                published_ref,
                publisher_commit,
                ..
            }) if published_ref == "refs/heads/main" && !publisher_commit.is_empty()
        ));
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

        // Missing own overlays are lifecycle transients. One bounded inline
        // refresh should recover the session view from the checkout bytes.
        std::fs::write(
            repo.join(".bbox/gaps/gap-12345678.json"),
            serde_json::to_vec_pretty(&json!({
                "id": "gap-12345678",
                "title": "Dirty checkout gap",
                "gap_kind": "tooling",
                "domain": "tests",
                "wanted_capability": "cached published snapshots",
                "dedupe_key": "tooling/tests/cache-published-gap-reads",
                "created_at": "2026-01-01T00:00:00Z"
            }))
            .expect("serialize dirty gap"),
        )
        .expect("write dirty gap");
        let project_id = state.records_provider.records_snapshot().records[0]
            .project_id
            .clone();
        let own_checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&repo)
            .expect("mint own checkout identity");
        let own_checkout = ResolvedCheckoutScope {
            project_id,
            published_scope: scope.clone(),
            checkout_id: own_checkout_id.clone(),
            checkout_dir: repo.to_string_lossy().into_owned(),
            checkout_project_dir: repo.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/main".into()),
        };
        server
            .register_dark_knowledge_checkout(&own_checkout)
            .expect("register own checkout authority");
        server
            .session_checkout
            .set(Some(Arc::new(own_checkout)))
            .expect("pin checkout");
        let own = server
            .session_gap_view(Some(repo.to_str().expect("utf8 repo")), Some("own"))
            .expect("bounded refresh should recover missing own gap overlay");
        assert_eq!(own.gaps.all()[0].title, "Dirty checkout gap");
        assert_eq!(own.built_from.len(), 1);
        let own_ref = own
            .gaps
            .view_metadata("gap-12345678")
            .and_then(|metadata| metadata.built_from_ref.as_deref())
            .expect("own gap stamp ref");
        assert!(matches!(
            own.built_from.get(own_ref),
            Some(BuiltFromStamp::CheckoutOverlay {
                checkout_id,
                working_fingerprint,
                ..
            }) if checkout_id == &own_checkout_id && !working_fingerprint.is_empty()
        ));

        server.invalidate_published_knowledge_cache(&scope);
        assert!(state.gap_published_cache.read().is_empty());
        assert!(state.publisher_authorization_cache.read().is_empty());
    }

    #[test]
    fn provisional_gap_refs_include_checkout_identity() {
        assert_eq!(
            provisional_gap_ref("checkout-a", "gap-12345678"),
            "provisional_gap:checkout-a:gap-12345678"
        );
        assert_ne!(
            provisional_gap_ref("checkout-a", "gap-12345678"),
            provisional_gap_ref("checkout-b", "gap-12345678")
        );
    }
}

/// Catalog published gap views (Phase 5 plan section 8, P5-B).
#[cfg(test)]
mod catalog_view_tests {
    use crate::server::state::catalog_fixture::{
        COMMIT_ONE, COMMIT_TWO, CatalogFixture, gap_note, knowledge_entry,
    };

    use super::*;

    fn gap(view: &SessionGapView, id: &str) -> GapNote {
        view.gaps
            .all()
            .iter()
            .find(|gap| gap.id == id)
            .cloned()
            .expect("gap row is present")
    }

    fn published_stamp(view: &SessionGapView, id: &str) -> BuiltFromStamp {
        let reference = view
            .gaps
            .view_metadata(id)
            .and_then(|metadata| metadata.built_from_ref.clone())
            .expect("catalog published gap rows carry a built_from stamp");
        view.built_from
            .get(&reference)
            .cloned()
            .expect("the stamp reference resolves in the view table")
    }

    #[test]
    fn a_remote_only_catalog_project_serves_accepted_gaps_with_no_lease() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_remote", &scope);
        fixture.install_publication(
            "p_remote",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "accepted content")],
            &[gap_note("gap-1234abcd", "accepted gap")],
        );
        let server = fixture.server();

        let view = server.session_gap_view(None, None).unwrap();
        let row = gap(&view, "gap-1234abcd");
        assert_eq!(row.title, "accepted gap");
        assert_eq!(row.project_id.as_deref(), Some("p_remote"));
        // The host-local carrier fields belong to a checkout, and a catalog
        // published read has none.
        assert_eq!(row.project, None);
        assert_eq!(row.write_dir, None);
        assert_eq!(row.provisional_checkout_id, None);
        assert_eq!(
            published_stamp(&view, "gap-1234abcd"),
            BuiltFromStamp::Published {
                published_scope: scope,
                published_ref: "refs/heads/main".into(),
                publisher_commit: COMMIT_ONE.into(),
            }
        );

        let health = server.state.checkout_access.health();
        assert!(
            health
                .operations
                .iter()
                .all(|operation| operation.granted == 0 && operation.denied == 0)
        );
    }

    #[test]
    fn accepted_gap_rows_survive_a_fully_detached_binding() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_detached", &scope);
        fixture.install_publication(
            "p_detached",
            &scope,
            COMMIT_ONE,
            &[],
            &[gap_note("gap-1234abcd", "still readable")],
        );
        let server = fixture.server();

        // The pointer names an attachment id, and the catalog holds no
        // attachment at all: this is the detached binding, and the accepted
        // bytes must keep serving through it.
        assert!(
            server
                .state
                .project_authority
                .catalog_store()
                .unwrap()
                .snapshot()
                .unwrap()
                .attachments()
                .attachments
                .is_empty()
        );
        let view = server.session_gap_view(None, None).unwrap();
        assert_eq!(gap(&view, "gap-1234abcd").title, "still readable");
    }

    #[test]
    fn a_project_without_a_pointer_reports_unavailable_gaps() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project("p_nopublication", &CatalogFixture::scope("."));
        let server = fixture.server();

        let view = server.session_gap_view(None, None).unwrap();
        assert!(view.gaps.all().is_empty());
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.contains("p_nopublication")
                    && diagnostic.contains("no accepted publication pointer")
            }),
            "{:?}",
            view.diagnostics
        );
    }

    #[test]
    fn the_gap_content_cache_is_keyed_by_accepted_content_identity() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_cache", &scope);
        fixture.install_publication(
            "p_cache",
            &scope,
            COMMIT_ONE,
            &[],
            &[gap_note("gap-1234abcd", "generation one")],
        );
        let server = fixture.server();
        let project_id = ProjectId::parse("p_cache").unwrap();

        server.session_gap_view(None, None).unwrap();
        let first_stamp = server
            .state
            .catalog_gap_published_cache
            .read()
            .get(&project_id)
            .expect("the first read installs a projected gap snapshot")
            .content_stamp
            .clone();

        fixture.install_publication(
            "p_cache",
            &scope,
            COMMIT_TWO,
            &[],
            &[gap_note("gap-1234abcd", "generation two")],
        );
        server.invalidate_catalog_published_content(&project_id);
        let after = server.session_gap_view(None, None).unwrap();
        assert_eq!(gap(&after, "gap-1234abcd").title, "generation two");
        assert_ne!(
            server
                .state
                .catalog_gap_published_cache
                .read()
                .get(&project_id)
                .unwrap()
                .content_stamp,
            first_stamp
        );
    }
}

/// Catalog gap overlay baseline path (Phase 5 plan sections 8 P5-D and
/// 13.4). The knowledge twin covers the shared lease, attachment, and
/// accepted-identity plumbing; these cover the gap lane's own diff, row
/// stamping, and store.
#[cfg(test)]
mod catalog_gap_overlay_tests {
    use std::path::PathBuf;
    use std::process::Command;

    use crate::server::state::catalog_fixture::{CatalogFixture, gap_note, knowledge_entry};

    use super::*;

    const PROJECT: &str = "p_gapoverlay";
    const BASE_ATTACHMENT: &str = "att_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb01";
    const BASE_CHECKOUT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb01";
    const PEER_ATTACHMENT: &str = "att_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02";
    const PEER_CHECKOUT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02";

    fn git_run(root: &Path, args: &[&str]) {
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

    /// Write one gap with the exact bytes the fixture's accepted
    /// publication hashes. An accepted generation records the SHA-256 of
    /// the committed blob, so committing one serialization and publishing
    /// another would silently disable the byte-equality suppression rule.
    fn write_gap(root: &Path, gap: &GapNote) {
        let dir = root.join(".bbox/gaps");
        std::fs::create_dir_all(&dir).unwrap();
        // Write exactly what a writer commits. Encoding these
        // independently was how this suite went vacuously green: the
        // fixture and the test shared one private encoding, so a
        // suppression assertion compared fixture bytes to fixture bytes
        // and never touched the bytes production writes.
        std::fs::write(
            dir.join(format!("{}.json", gap.id)),
            bbox_gaps::gaps::committed_gap_note_bytes(gap).unwrap(),
        )
        .unwrap();
    }

    fn edited(id: &str, title: &str) -> GapNote {
        let mut gap = gap_note(id, title);
        gap.notes = Some(title.to_string());
        gap
    }

    struct GapOverlayFixture {
        catalog: CatalogFixture,
        _temp: tempfile::TempDir,
        root: PathBuf,
        base: PathBuf,
        accepted_commit: String,
        scope: PublishedScope,
    }

    impl GapOverlayFixture {
        fn new(gaps: &[GapNote]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let base = root.join("base");
            std::fs::create_dir_all(&base).unwrap();
            git_run(&base, &["init", "-q", "-b", "main"]);
            git_run(&base, &["config", "user.email", "t@example.com"]);
            git_run(&base, &["config", "user.name", "Test"]);
            for gap in gaps {
                write_gap(&base, gap);
            }
            git_run(&base, &["add", ".bbox/gaps"]);
            git_run(&base, &["commit", "-q", "-m", "accepted"]);
            let accepted_commit = bbox_corpus_core::git::current_head(&base).unwrap();

            let catalog = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            catalog.add_published_project(PROJECT, &scope);
            catalog.install_publication(
                PROJECT,
                &scope,
                &accepted_commit,
                &[knowledge_entry("knowledge-a", "accepted")],
                gaps,
            );
            catalog.attach_overlay_checkout(
                PROJECT,
                &scope,
                &base,
                BASE_ATTACHMENT,
                BASE_CHECKOUT,
                true,
            );
            Self {
                catalog,
                _temp: temp,
                root,
                base,
                accepted_commit,
                scope,
            }
        }

        fn worktree(&self, name: &str, attachment_id: &str, checkout_id: &str) -> PathBuf {
            let path = self.root.join(name);
            git_run(
                &self.base,
                &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
            );
            self.catalog.attach_overlay_checkout(
                PROJECT,
                &self.scope,
                &path,
                attachment_id,
                checkout_id,
                true,
            );
            path
        }

        /// A repository with unrelated history: it cannot contain the
        /// accepted commit, and there is no publisher root to borrow it
        /// from (D-007).
        fn unrelated(&self, name: &str, attachment_id: &str, checkout_id: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            git_run(&path, &["init", "-q", "-b", "main"]);
            git_run(&path, &["config", "user.email", "t@example.com"]);
            git_run(&path, &["config", "user.name", "Test"]);
            write_gap(&path, &gap_note("gap-unrelated", "unrelated"));
            git_run(&path, &["add", ".bbox/gaps"]);
            git_run(&path, &["commit", "-q", "-m", "unrelated"]);
            self.catalog.attach_overlay_checkout(
                PROJECT,
                &self.scope,
                &path,
                attachment_id,
                checkout_id,
                true,
            );
            path
        }

        fn project_id(&self) -> ProjectId {
            ProjectId::parse(PROJECT).unwrap()
        }

        fn verified(&self, server: &BlackboxServer) -> VerifiedAcceptedPublication {
            server
                .state
                .accepted_publications
                .as_ref()
                .unwrap()
                .load_verified(&self.project_id())
                .unwrap()
        }
    }

    fn attachment(attachment_id: &str, checkout_id: &str) -> CatalogOverlayAttachment {
        CatalogOverlayAttachment {
            attachment_id: attachment_id.to_string(),
            checkout_id: checkout_id.to_string(),
        }
    }

    #[test]
    fn an_attached_checkout_positions_its_gaps_against_accepted_content() {
        let fixture = GapOverlayFixture::new(&[
            gap_note("gap-11111111", "accepted"),
            gap_note("gap-22222222", "removed in the checkout"),
        ]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_gap(
            &worktree,
            &edited("gap-11111111", "changed in the checkout"),
        );
        std::fs::remove_file(worktree.join(".bbox/gaps/gap-22222222.json")).unwrap();

        let server = fixture.catalog.server_with_checkout_authority();
        server.set_session_checkout_for_test(
            PROJECT.into(),
            fixture.scope.clone(),
            PEER_CHECKOUT.into(),
            worktree,
        );

        let view = server.session_gap_view(None, Some("own")).unwrap();
        let changed = view
            .gaps
            .all()
            .iter()
            .find(|gap| gap.id == "gap-11111111")
            .expect("the checkout variant replaces the published row");
        assert_eq!(changed.notes.as_deref(), Some("changed in the checkout"));
        // A catalog overlay row carries durable identity and its checkout,
        // never a host path.
        assert_eq!(changed.project, None);
        assert_eq!(changed.project_id.as_deref(), Some(PROJECT));
        assert_eq!(
            changed.provisional_checkout_id.as_deref(),
            Some(PEER_CHECKOUT)
        );
        assert!(
            view.gaps.all().iter().all(|gap| gap.id != "gap-22222222"),
            "a tombstoned gap leaves the own view"
        );

        let stamp_ref = view
            .gaps
            .view_metadata("gap-11111111")
            .and_then(|row| row.built_from_ref.clone())
            .expect("an overlay row carries a provable stamp");
        let BuiltFromStamp::CheckoutOverlay {
            checkout_id,
            publisher_commit,
            merge_base,
            ..
        } = view.built_from.get(&stamp_ref).cloned().unwrap()
        else {
            panic!("an overlay row stamps CheckoutOverlay");
        };
        assert_eq!(checkout_id, PEER_CHECKOUT);
        assert_eq!(publisher_commit, fixture.accepted_commit);
        assert_eq!(merge_base, fixture.accepted_commit);
    }

    /// The digest map the diff consults is keyed by BASENAME while the
    /// accepted manifest keys are repository-relative. Feeding a manifest
    /// key straight through makes every published gap look absent, which
    /// suppresses nothing and tombstones nothing: wrong answers rather
    /// than errors. A published scope below the repository root gives the
    /// manifest key a real directory prefix.
    #[test]
    fn a_nested_published_scope_still_suppresses_and_tombstones_by_basename() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let base = root.join("base");
        let nested = |dir: &Path| dir.join("sub");
        std::fs::create_dir_all(nested(&base)).unwrap();
        git_run(&base, &["init", "-q", "-b", "main"]);
        git_run(&base, &["config", "user.email", "t@example.com"]);
        git_run(&base, &["config", "user.name", "Test"]);

        // A commit before accepted content, so the checkout's baseline and
        // accepted content genuinely disagree and the suppression question
        // is actually asked.
        write_gap(&nested(&base), &edited("gap-11111111", "older"));
        write_gap(&nested(&base), &gap_note("gap-22222222", "published"));
        git_run(&base, &["add", "sub/.bbox/gaps"]);
        git_run(&base, &["commit", "-q", "-m", "before accepted"]);
        let branch_point = bbox_corpus_core::git::current_head(&base).unwrap();

        let accepted = [
            edited("gap-11111111", "accepted"),
            gap_note("gap-22222222", "published"),
        ];
        write_gap(&nested(&base), &accepted[0]);
        git_run(&base, &["add", "sub/.bbox/gaps"]);
        git_run(&base, &["commit", "-q", "-m", "accepted"]);
        let accepted_commit = bbox_corpus_core::git::current_head(&base).unwrap();

        let catalog = CatalogFixture::new();
        let scope = CatalogFixture::scope("sub");
        catalog.add_published_project(PROJECT, &scope);
        catalog.install_publication(
            PROJECT,
            &scope,
            &accepted_commit,
            &[knowledge_entry("knowledge-a", "accepted")],
            &accepted,
        );

        let worktree = root.join("peer");
        git_run(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "peer",
                worktree.to_str().unwrap(),
                &branch_point,
            ],
        );
        catalog.attach_overlay_checkout_at(
            PROJECT,
            &scope,
            &worktree,
            &nested(&worktree),
            PEER_ATTACHMENT,
            PEER_CHECKOUT,
            true,
        );

        // Re-apply exactly what accepted content holds: the baseline still
        // disagrees, so only the published digest can suppress this row.
        write_gap(&nested(&worktree), &edited("gap-11111111", "accepted"));
        std::fs::remove_file(nested(&worktree).join(".bbox/gaps/gap-22222222.json")).unwrap();

        let server = catalog.server_with_checkout_authority();
        let verified = server
            .state
            .accepted_publications
            .as_ref()
            .unwrap()
            .load_verified(&ProjectId::parse(PROJECT).unwrap())
            .unwrap();
        assert!(
            verified
                .gap_manifest()
                .keys()
                .all(|filename| filename.as_str().starts_with("sub/.bbox/gaps/")),
            "the fixture must produce directory-prefixed manifest keys"
        );

        let snapshot = server
            .refresh_catalog_gap_overlay(&verified, &attachment(PEER_ATTACHMENT, PEER_CHECKOUT))
            .unwrap();
        assert_eq!(snapshot.stamp.as_ref().unwrap().merge_base, branch_point);
        assert!(
            !snapshot.values.contains_key("gap-11111111"),
            "working bytes equal to published content are already integrated: {:?}",
            snapshot.values
        );
        assert!(
            matches!(
                snapshot.values.get("gap-22222222"),
                Some(GapOverlayValue::Tombstone)
            ),
            "a deletion of a published gap tombstones it: {:?}",
            snapshot.values
        );
    }

    #[test]
    fn a_detached_attachment_refuses_own_while_published_gaps_keep_serving() {
        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "accepted")]);
        fixture.catalog.detach(BASE_ATTACHMENT);
        let server = fixture.catalog.server_with_checkout_authority();
        server.set_session_checkout_for_test(
            PROJECT.into(),
            fixture.scope.clone(),
            BASE_CHECKOUT.into(),
            fixture.base.clone(),
        );

        let error = server
            .session_gap_view(None, Some("own"))
            .err()
            .expect("own has no honest answer without its own checkout");
        let text = format!("{error:#}");
        assert!(
            text.contains(ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE),
            "{text}"
        );
        assert!(text.contains("attachment_inactive"), "{text}");

        let published = server.session_gap_view(None, Some("published")).unwrap();
        assert!(
            published
                .gaps
                .all()
                .iter()
                .any(|gap| gap.id == "gap-11111111")
        );
        assert!(
            published
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.contains("overlay")),
            "published ignores overlay failure entirely: {:?}",
            published.diagnostics
        );
    }

    #[test]
    fn all_omits_only_the_failed_gap_peer_and_reports_its_reason() {
        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "accepted")]);
        let peer = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_gap(&peer, &edited("gap-11111111", "peer variant"));
        const BROKEN_ATTACHMENT: &str = "att_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb03";
        const BROKEN_CHECKOUT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb03";
        fixture.unrelated("broken", BROKEN_ATTACHMENT, BROKEN_CHECKOUT);

        let server = fixture.catalog.server_with_checkout_authority();
        let view = server.session_gap_view(None, Some("all")).unwrap();

        // Accepted content and the healthy peer both keep serving, and the
        // peer's variant is a distinct row rather than a replacement.
        assert!(view.gaps.all().iter().any(|gap| gap.id == "gap-11111111"));
        let peer_row = view
            .gaps
            .all()
            .iter()
            .find(|gap| gap.id == format!("provisional_gap:{PEER_CHECKOUT}:gap-11111111"))
            .expect("a healthy peer contributes its variant");
        assert_eq!(peer_row.notes.as_deref(), Some("peer variant"));
        assert_eq!(peer_row.project_id.as_deref(), Some(PROJECT));

        let reason = view
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.contains(ERROR_OVERLAY_BASELINE_UNAVAILABLE))
            .unwrap_or_else(|| panic!("the omitted peer is reported: {:?}", view.diagnostics));
        assert!(reason.contains(BROKEN_CHECKOUT), "{reason}");
        assert!(reason.contains(BROKEN_ATTACHMENT), "{reason}");
        assert!(
            !reason.contains(fixture.root.to_str().unwrap()),
            "a degradation must not carry an absolute path: {reason}"
        );
    }

    /// Plan section 10.5: `all` retains accepted content and reports every
    /// omitted peer through bounded `degraded.overlays`. The knowledge lane
    /// already served typed rows; this proves the gap response does too,
    /// through the tool that actually assembles it rather than through the
    /// view struct alone.
    #[test]
    fn the_gap_response_serializes_every_omitted_peer_as_a_typed_row() {
        use crate::gaps::GapListParams;
        use rmcp::handler::server::wrapper::Parameters;

        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "accepted")]);
        let peer = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_gap(&peer, &edited("gap-11111111", "peer variant"));
        const BROKEN_ATTACHMENT: &str = "att_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb04";
        const BROKEN_CHECKOUT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb04";
        fixture.unrelated("broken", BROKEN_ATTACHMENT, BROKEN_CHECKOUT);

        let server = fixture.catalog.server_with_checkout_authority();
        let response = server.bbox_gaps(Parameters(GapListParams {
            provisional: Some("all".into()),
            include_addressed: Some(true),
            json: Some(true),
            ..Default::default()
        }));
        assert_ne!(response.is_error, Some(true), "{response:?}");
        let structured = response
            .structured_content
            .expect("bbox_gaps structured response");

        let overlays = structured["degraded"]["overlays"]
            .as_array()
            .unwrap_or_else(|| panic!("degraded.overlays is present: {structured}"));
        assert_eq!(overlays.len(), 1, "{structured}");
        assert_eq!(overlays[0]["code"], ERROR_OVERLAY_BASELINE_UNAVAILABLE);
        assert_eq!(overlays[0]["checkout_id"], BROKEN_CHECKOUT);
        assert_eq!(overlays[0]["attachment_id"], BROKEN_ATTACHMENT);
        assert_eq!(overlays[0]["project_id"], PROJECT);

        // Accepted content still serves, which is what makes this a
        // degradation rather than a failure.
        assert!(
            structured["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == "gap-11111111"),
            "{structured}"
        );
        // The human rendering is retained beside the typed rows, not
        // replaced by them.
        assert!(
            structured["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line
                    .as_str()
                    .is_some_and(|line| line.contains(ERROR_OVERLAY_BASELINE_UNAVAILABLE))),
            "{structured}"
        );
        // A degradation names identity and a stable code, never a path.
        assert!(
            !structured
                .to_string()
                .contains(fixture.root.to_str().unwrap()),
            "a degradation must not carry an absolute path: {structured}"
        );
    }

    #[test]
    fn an_advance_during_capture_refuses_the_gap_snapshot() {
        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "generation one")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_gap(&worktree, &edited("gap-11111111", "checkout variant"));
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);

        let degradation = server
            .compute_catalog_gap_overlay(
                &verified,
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
                || {
                    fixture.catalog.install_publication(
                        PROJECT,
                        &fixture.scope,
                        &fixture.accepted_commit,
                        &[knowledge_entry("knowledge-a", "accepted")],
                        &[gap_note("gap-11111111", "generation two")],
                    );
                    server.invalidate_catalog_published_content(&fixture.project_id());
                },
            )
            .err()
            .expect("a snapshot may not position a checkout against unpublished bytes");
        assert_eq!(degradation.code, ERROR_OVERLAY_ACCEPTED_CONTENT_CHANGED);
        assert!(!degradation.transient);
    }

    #[test]
    fn a_detach_during_capture_fails_gap_lease_revalidation() {
        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "accepted")]);
        fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);

        let degradation = server
            .compute_catalog_gap_overlay(
                &verified,
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
                || CatalogFixture::detach_in_server(&server, PEER_ATTACHMENT),
            )
            .err()
            .expect("bytes captured under a lease that no longer holds are unpublishable");
        assert_eq!(
            degradation.code,
            bbox_indexing::checkout_access::CheckoutAccessErrorCode::AttachmentInactive.as_str()
        );
        assert!(!degradation.transient);
    }

    /// Plan section 4.12: a checkout that cannot prove the baseline is a
    /// structural authority fact. Only a transient failure may hold a
    /// prior valid snapshot open.
    #[test]
    fn a_structural_gap_failure_replaces_a_prior_snapshot_that_a_transient_one_preserves() {
        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "accepted")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_gap(&worktree, &edited("gap-11111111", "checkout variant"));
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);
        let peer = attachment(PEER_ATTACHMENT, PEER_CHECKOUT);

        let first = server
            .refresh_catalog_gap_overlay(&verified, &peer)
            .unwrap();
        assert_eq!(first.status, GapOverlayStatus::Valid);

        // A transient refusal keeps the prior valid snapshot open for a
        // bounded window: the checkout is busy, not wrong.
        let busy = server
            .state
            .checkout_access
            .lifecycle_mutation_guard()
            .unwrap();
        let preserved = server
            .refresh_catalog_gap_overlay(&verified, &peer)
            .unwrap();
        assert_eq!(preserved.snapshot_id, first.snapshot_id);
        drop(busy);

        // The same checkout on an orphan branch still contains commit P
        // but shares no ancestor with it: no baseline exists.
        git_run(&worktree, &["checkout", "-q", "--orphan", "orphaned"]);
        git_run(&worktree, &["add", ".bbox/gaps"]);
        git_run(&worktree, &["commit", "-q", "-m", "orphan"]);

        let degradation = server
            .refresh_catalog_gap_overlay(&verified, &peer)
            .err()
            .expect("a structural failure has no valid answer");
        assert_eq!(degradation.code, ERROR_OVERLAY_BASELINE_UNAVAILABLE);
        assert!(!degradation.transient);

        let stored = server
            .state
            .gap_overlays
            .read()
            .get(&fixture.scope, PEER_CHECKOUT)
            .cloned()
            .expect("the store keeps a record of the outcome");
        assert_eq!(
            stored.status,
            GapOverlayStatus::Invalid,
            "a structural failure must replace the prior snapshot, never preserve it"
        );
        assert!(stored.values.is_empty());
    }

    #[test]
    fn a_gap_checkout_that_never_settles_during_capture_is_transient() {
        let fixture = GapOverlayFixture::new(&[gap_note("gap-11111111", "accepted")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);
        let published = accepted_gap_digests(&verified);
        let content_stamp = verified.content_stamp();
        let lease = server
            .acquire_catalog_overlay_lease(
                &fixture.project_id(),
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
            )
            .unwrap();

        let mut revision = 0;
        let error = stable_catalog_gap_overlay(
            CatalogGapOverlayPublished {
                published_scope: &fixture.scope,
                checkout_id: PEER_CHECKOUT,
                full_ref: content_stamp.full_ref(),
                accepted_commit: content_stamp.accepted_commit(),
                accepted_generation: content_stamp.generation_id(),
                published: &published,
            },
            &lease,
            || {
                revision += 1;
                write_gap(
                    &worktree,
                    &edited("gap-11111111", &format!("edit {revision}")),
                );
            },
        )
        .err()
        .expect("a checkout that keeps moving is busy, not positioned");
        assert_eq!(error.kind, GapOverlayRecomputeErrorKind::Transient);
        assert!(!error.is_structural());
    }
}
