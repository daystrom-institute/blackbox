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
    GapOverlayKey, GapOverlayRecomputeError, GapOverlayRecomputeErrorKind, GapOverlaySnapshot,
    GapOverlayStatus, GapOverlayValue, GapTransientPreservationOutcome, PublishedGapEntry,
    PublishedGapSnapshot, WorkingGapSnapshot, load_published_snapshot_at_commit,
    recompute_overlay_result,
};
use bbox_indexing::accepted_publication_runtime::{
    AcceptedBlockingLevelV1, AcceptedGapEntryV1, AcceptedGapImpactV1, AcceptedGapKindV1,
    AcceptedGapResolutionV1, AcceptedPublicationContentStamp, VerifiedAcceptedPublication,
};
use bbox_knowledge::overlay::ProvisionalMode;

use super::BlackboxServer;

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
        let mode = ProvisionalMode::parse(provisional, session_checkout.is_some())?;
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
                &mut gaps,
                &mut metadata,
                &mut built_from,
                &mut diagnostics,
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
        gaps: &mut Vec<bbox_gaps::gaps::GapNote>,
        metadata: &mut BTreeMap<String, GapViewMetadata>,
        built_from: &mut BuiltFromTable,
        diagnostics: &mut Vec<String>,
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
                    diagnostics.push(super::knowledge_view::catalog_publication_diagnostic(
                        target.project_id.as_str(),
                        &error,
                    ));
                    continue;
                }
            };
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
            for (id, entry) in published.gaps {
                metadata.insert(
                    id,
                    GapViewMetadata {
                        built_from_ref: Some(published_ref.clone()),
                        compatibility_lane: None,
                    },
                );
                gaps.push(entry.gap);
            }
            if mode != ProvisionalMode::Published {
                diagnostics.push(format!(
                    "project {}: provisional gap overlays for catalog projects land with the \
                     phase-5 catalog overlay baseline path",
                    target.project_id
                ));
            }
        }
        Ok(())
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
