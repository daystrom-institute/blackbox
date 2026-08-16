use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bbox_corpus_core::built_from::{BuiltFromStamp, BuiltFromTable};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{AttachmentStatus, ProjectId};
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::accepted_publication_runtime::{
    AcceptedEdgeConfidenceV1, AcceptedKnowledgeApprovalV1, AcceptedKnowledgeCategoryV1,
    AcceptedKnowledgeEdgeKindV1, AcceptedKnowledgeEntryV1, AcceptedKnowledgePriorityV1,
    AcceptedKnowledgeStatusV1, AcceptedPublicationContentStamp, AcceptedPublicationRuntimeError,
    AcceptedPublicationScopeAgreement, AcceptedPublicationSelection,
    ERROR_ACCEPTED_PUBLICATION_MISSING, VerifiedAcceptedPublication,
};
use bbox_indexing::checkout_access::{
    CheckoutAccessError, CheckoutAccessErrorCode, CheckoutAccessIntent, CheckoutAccessKind,
    CheckoutAccessRequest, CheckoutAccessSourceLane, CheckoutAttachmentSelector,
    ValidatedCheckoutLease,
};
use bbox_knowledge::knowledge::{
    Approval, Category, Knowledge, KnowledgeEdge, KnowledgeEdgeKind, KnowledgeEntry,
    KnowledgeViewMetadata, Priority, Scope, Status,
};
use bbox_knowledge::overlay::{
    AcceptedPublishedDigests, CatalogOverlayPublished, OverlayKey, OverlayRecomputeError,
    OverlayRecomputeErrorKind, OverlaySnapshot, OverlayStatus, OverlayValue, ProvisionalMode,
    PublishedKnowledgeEntry, PublishedKnowledgeSnapshot, TransientPreservationOutcome,
    WorkingKnowledgeSnapshot, load_published_snapshot_at_commit_unhydrated, provisional_entity_ref,
    recompute_catalog_overlay_result,
};

use super::{BlackboxServer, SharedState};

#[derive(Clone)]
pub(crate) struct PublishedKnowledgeCacheEntry {
    publisher_project_id: String,
    publisher_commit: String,
    durable_project: String,
    snapshot: PublishedKnowledgeSnapshot,
}

/// One catalog project's projected accepted knowledge, valid exactly while
/// its accepted content identity is unchanged. Keyed by project rather than
/// by stamp so the map stays bounded by the catalog: an advance replaces the
/// entry instead of accumulating one per generation.
#[derive(Clone)]
pub(crate) struct CatalogPublishedKnowledgeCacheEntry {
    pub(crate) content_stamp: AcceptedPublicationContentStamp,
    pub(crate) snapshot: PublishedKnowledgeSnapshot,
}

/// Compatibility-lane tag carried by view rows served without a provable
/// `built_from` stamp (legacy loaded state, unstamped checkout overlays).
pub(crate) const LEGACY_COMPATIBILITY_LANE: &str = "legacy_compatibility";

/// The single diagnostic the legacy knowledge lane emits. Named so a
/// response can decide whether its OWN rows warrant it (gap-40ab1102): the
/// line describes rows, and firing it on a fully-stamped result set trains
/// callers to ignore diagnostics.
pub(crate) const LEGACY_COMPATIBILITY_KNOWLEDGE_DIAGNOSTIC: &str =
    "legacy_compatibility knowledge rows have no provable built_from stamp";

#[derive(Debug, Clone)]
pub(crate) struct KnowledgeViewItem {
    pub(crate) entity_ref: String,
    pub(crate) entry: KnowledgeEntry,
    pub(crate) metadata: KnowledgeViewMetadata,
}

/// What one startup published-index reconciliation pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StartupConvergenceReport {
    pub(crate) visited: usize,
    pub(crate) converged: usize,
    /// Projects with no verified accepted content. Their index rows are
    /// left alone rather than cleared: a prior-generation fallback may
    /// still be serving them.
    pub(crate) skipped: usize,
}

pub(crate) struct SessionKnowledgeView {
    pub(crate) knowledge: Knowledge,
    pub(crate) items: Vec<KnowledgeViewItem>,
    pub(crate) built_from: BuiltFromTable,
    pub(crate) diagnostics: Vec<String>,
    /// Checkouts `all` omitted because they could not position themselves
    /// against accepted content. Empty in every other mode: `published`
    /// ignores overlay failure and `own` refuses instead of omitting.
    pub(crate) degraded_overlays: Vec<OverlayDegradation>,
}

impl SessionKnowledgeView {
    pub(crate) fn append_built_from_for_ids(
        &self,
        output: String,
        returned_ids: &[String],
    ) -> String {
        let refs = returned_ids.iter().filter_map(|id| {
            self.knowledge
                .view_metadata(id)
                .and_then(|metadata| metadata.built_from_ref.as_deref())
        });
        let table = self.built_from_for_refs(refs);
        self.append_built_from_table(output, &table)
    }

    pub(crate) fn append_list_built_from(&self, output: String) -> String {
        let returned_ids = output
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix('[')?;
                let end = rest.find(']')?;
                let id = rest[..end].trim();
                (!id.is_empty()).then(|| id.to_string())
            })
            .collect::<Vec<_>>();
        self.append_built_from_for_ids(output, &returned_ids)
    }

    pub(crate) fn metadata_for_entity_ref(
        &self,
        entity_ref: &str,
    ) -> Option<&KnowledgeViewMetadata> {
        let key = entity_ref.strip_prefix("knowledge:").unwrap_or(entity_ref);
        self.knowledge.view_metadata(key)
    }

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

    pub(crate) fn enrich_json_response(
        &self,
        output: String,
    ) -> Result<(String, serde_json::Value)> {
        let mut structured: serde_json::Value = serde_json::from_str(&output)
            .context("parsing knowledge-bearing response for built_from wiring")?;
        let mut row_stamps = Vec::<(String, String)>::new();
        let mut used_stamp_refs = Vec::<String>::new();
        self.enrich_json_value(&mut structured, &mut row_stamps, &mut used_stamp_refs);
        let built_from = self.built_from_for_refs(used_stamp_refs.iter().map(String::as_str));
        if let Some(object) = structured.as_object_mut() {
            if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
                let mut text = text.to_string();
                append_row_stamp_refs(&mut text, &row_stamps);
                text = self.append_built_from_table(text, &built_from);
                object.insert("text".into(), serde_json::Value::String(text));
            }
            object.insert("built_from".into(), serde_json::to_value(&built_from)?);
        }
        let rendered = serde_json::to_string_pretty(&structured)?;
        Ok((rendered, structured))
    }

    fn enrich_json_value(
        &self,
        value: &mut serde_json::Value,
        row_stamps: &mut Vec<(String, String)>,
        used_stamp_refs: &mut Vec<String>,
    ) {
        match value {
            serde_json::Value::Object(object) => {
                let entity_ref = object
                    .get("entity_ref")
                    .or_else(|| object.get("entity_id"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|entity_ref| {
                        entity_ref.starts_with("knowledge:")
                            || entity_ref.starts_with("provisional_knowledge:")
                    })
                    .map(str::to_owned);
                if let Some(entity_ref) = entity_ref
                    && let Some(metadata) = self.metadata_for_entity_ref(&entity_ref)
                {
                    if let Some(reference) = &metadata.built_from_ref {
                        object.insert(
                            "built_from_ref".into(),
                            serde_json::Value::String(reference.clone()),
                        );
                        row_stamps.push((entity_ref, reference.clone()));
                        used_stamp_refs.push(reference.clone());
                    } else if let Some(lane) = &metadata.compatibility_lane {
                        object.insert(
                            "compatibility_lane".into(),
                            serde_json::Value::String(lane.clone()),
                        );
                        row_stamps.push((entity_ref, lane.clone()));
                    }
                }
                for child in object.values_mut() {
                    self.enrich_json_value(child, row_stamps, used_stamp_refs);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    self.enrich_json_value(child, row_stamps, used_stamp_refs);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn structured_response(&self, returned_ids: &[String]) -> serde_json::Value {
        let rows = returned_ids
            .iter()
            .filter_map(|id| {
                let entry = self.knowledge.entry(id)?;
                let metadata = self.knowledge.view_metadata(id);
                let entity_ref = if id.starts_with("provisional_knowledge:") {
                    id.clone()
                } else {
                    format!("knowledge:{id}")
                };
                Some(serde_json::json!({
                    "entity_ref": entity_ref,
                    "entry": entry,
                    "built_from_ref": metadata.and_then(|row| row.built_from_ref.as_deref()),
                    "compatibility_lane": metadata.and_then(|row| row.compatibility_lane.as_deref()),
                }))
            })
            .collect::<Vec<_>>();
        let refs = returned_ids.iter().filter_map(|id| {
            self.knowledge
                .view_metadata(id)
                .and_then(|metadata| metadata.built_from_ref.as_deref())
        });
        let built_from = self.built_from_for_refs(refs);
        let mut response = serde_json::json!({
            "rows": rows,
            "built_from": built_from,
            "diagnostics": &self.diagnostics,
        });
        // Bounded structured degradation for `all` (plan section 10.5).
        // Omitted entirely when nothing degraded, so a bridge response and
        // a healthy catalog response keep their existing shape.
        if !self.degraded_overlays.is_empty() {
            response["degraded"] = serde_json::json!({ "overlays": &self.degraded_overlays });
        }
        response
    }

    /// Shape this view's diagnostics for ONE response (gap-40ab1102).
    ///
    /// Two rules. The legacy-compatibility line describes ROWS, so it rides
    /// a response only when the rows that response returned actually include
    /// a legacy-lane row; a view-wide legacy row the caller never saw (a
    /// global entry, another project's leftovers) must not fire it, because
    /// a diagnostic that fires on every fully-stamped result set trains
    /// callers to ignore diagnostics. Filter-resolution diagnostics lead the
    /// list, because they are what explains an empty result.
    pub(crate) fn finalize_response_diagnostics(
        &mut self,
        returned_legacy_rows: bool,
        filter_diagnostics: Vec<String>,
    ) {
        if !returned_legacy_rows {
            self.diagnostics.retain(|diagnostic| {
                diagnostic.as_str() != LEGACY_COMPATIBILITY_KNOWLEDGE_DIAGNOSTIC
            });
        }
        let mut diagnostics = filter_diagnostics;
        diagnostics.append(&mut self.diagnostics);
        self.diagnostics = diagnostics;
    }

    /// Whether any of these returned rows came from the legacy lane.
    pub(crate) fn returned_rows_include_legacy_lane(&self, returned_ids: &[String]) -> bool {
        returned_ids.iter().any(|id| {
            self.knowledge
                .view_metadata(id)
                .and_then(|metadata| metadata.compatibility_lane.as_deref())
                == Some(LEGACY_COMPATIBILITY_LANE)
        })
    }

    /// The rendered diagnostics block. The header wording is frozen by the
    /// bridge parity capture (`tests/fixtures/bridge-parity`), so filter and
    /// overlay diagnostics share it rather than growing a second section.
    pub(crate) fn diagnostics_text(&self) -> Option<String> {
        (!self.diagnostics.is_empty()).then(|| {
            format!(
                "provisional visibility degraded:\n- {}",
                self.diagnostics.join("\n- ")
            )
        })
    }

    pub(crate) fn append_diagnostics(&self, output: String) -> String {
        match self.diagnostics_text() {
            Some(diagnostics) => format!("{output}\n{diagnostics}"),
            None => output,
        }
    }
}

fn append_row_stamp_refs(output: &mut String, row_stamps: &[(String, String)]) {
    if row_stamps.is_empty() {
        return;
    }
    output.push_str("\nKnowledge row built_from refs:\n");
    for (entity_ref, reference) in row_stamps {
        output.push_str("- ");
        output.push_str(entity_ref);
        output.push_str(" => ");
        output.push_str(reference);
        output.push('\n');
    }
}

impl BlackboxServer {
    pub(crate) fn authoritative_session_checkout(&self) -> Option<Arc<ResolvedCheckoutScope>> {
        self.session_checkout.get().and_then(Clone::clone)
    }

    pub(crate) fn authoritative_session_workspace_binding(
        &self,
    ) -> Option<Arc<super::knowledge_source::WorkspaceBindingGrant>> {
        self.session_workspace_binding.get().and_then(Clone::clone)
    }

    /// Drop committed-tree snapshots after a caller has already resolved and
    /// validated the current publisher authority.
    pub(crate) fn invalidate_published_snapshot_caches(&self, scope: &PublishedScope) {
        self.state.knowledge_published_cache.write().remove(scope);
        self.state.gap_published_cache.write().remove(scope);
    }

    /// Invalidate one scope's authority decision with generation protection so
    /// an already-running resolution cannot repopulate a stale result.
    pub(crate) fn invalidate_publisher_authority_cache(&self, scope: &PublishedScope) {
        self.state
            .publisher_authorization_cache
            .write()
            .invalidate(scope);
    }

    /// External publisher, registry, or ref movement invalidates both the
    /// authority decision and any snapshots derived from it.
    pub(crate) fn invalidate_published_knowledge_cache(&self, scope: &PublishedScope) {
        self.invalidate_published_snapshot_caches(scope);
        self.invalidate_publisher_authority_cache(scope);
    }

    #[cfg(test)]
    pub(crate) fn set_session_checkout_for_test(
        &self,
        project_id: String,
        published_scope: PublishedScope,
        checkout_id: String,
        checkout_dir: std::path::PathBuf,
    ) {
        self.session_checkout
            .set(Some(Arc::new(ResolvedCheckoutScope {
                project_id,
                published_scope,
                checkout_id,
                checkout_project_dir: checkout_dir.to_string_lossy().into_owned(),
                branch_ref: bbox_corpus_core::git::current_branch(&checkout_dir)
                    .map(|branch| format!("refs/heads/{branch}")),
                checkout_dir: checkout_dir.to_string_lossy().into_owned(),
            })))
            .unwrap();
    }

    /// Resolve one visibility decision and materialize the exact candidate set
    /// shared by list, search, inspection, and render consumers.
    pub(crate) fn session_knowledge_view(
        &self,
        requested_project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<SessionKnowledgeView> {
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
        let mut items = BTreeMap::<String, KnowledgeViewItem>::new();
        let mut built_from = BuiltFromTable::default();
        let mut diagnostics = Vec::new();
        let mut degraded_overlays = Vec::new();
        let mut has_legacy_compatibility_rows = false;
        for entry in self.state.kb.read().all_entries() {
            if self.path_fallback_is_cut() && entry.scope == Scope::Project {
                continue;
            }
            let is_managed_project = entry
                .project
                .as_deref()
                .is_some_and(|project| managed_paths.contains(project));
            if entry.scope == Scope::Project && is_managed_project {
                continue;
            }
            insert_published_item(
                &mut items,
                entry.clone(),
                None,
                None,
                None,
                Some(LEGACY_COMPATIBILITY_LANE),
            );
            has_legacy_compatibility_rows = true;
        }

        // Catalog published reads resolve durable project identity to a
        // verified accepted generation (plan section 4.1). They never enter
        // the version-1 lane below: no publisher election, no authorization
        // TTL, no publisher root, no Git, and no recall sidecar. Scoped and
        // unscoped reads take the same path, because a remote-only project
        // has no compatibility row to enumerate.
        let catalog_published = !self.state.project_authority.is_bridge();
        if catalog_published {
            self.append_catalog_published_knowledge(
                requested_project,
                requested_project_id.as_deref(),
                mode,
                session_checkout.as_deref(),
                session_workspace.as_deref(),
                &mut items,
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
                    // scope keep their legacy loaded knowledge view.
                    for entry in self.state.kb.read().all_entries().iter().filter(|entry| {
                        entry.scope == Scope::Project
                            && entry.project.as_deref() == Some(&project.canonical_path)
                    }) {
                        insert_published_item(
                            &mut items,
                            entry.clone(),
                            None,
                            None,
                            None,
                            Some(LEGACY_COMPATIBILITY_LANE),
                        );
                        has_legacy_compatibility_rows = true;
                    }
                }
                Ok(None) if explicit_managed_scope => {
                    anyhow::bail!(
                        "registered project {} has no authoritative published scope",
                        project.canonical_path
                    );
                }
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
            let published = self.cached_published_knowledge_snapshot(
                &publisher,
                &scope,
                &project.canonical_path,
            );
            let published = match published {
                Ok(published) => published,
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
            for published_entry in published.entries.into_values() {
                insert_published_item(
                    &mut items,
                    published_entry.entry,
                    Some(scope.clone()),
                    Some(published_entry.content_hash),
                    Some(&published_ref),
                    None,
                );
            }

            match mode {
                ProvisionalMode::Published => {}
                ProvisionalMode::Own => {
                    let Some(own) = session_checkout
                        .as_deref()
                        .filter(|own| own.published_scope == scope)
                    else {
                        continue;
                    };
                    let cached = {
                        self.state
                            .knowledge_overlays
                            .read()
                            .get(&scope, &own.checkout_id)
                            .cloned()
                    };
                    let snapshot = match cached {
                        Some(snapshot) => snapshot,
                        None => {
                            let _ = self.refresh_dark_knowledge_overlay(own);
                            self.state
                                .knowledge_overlays
                                .read()
                                .get(&scope, &own.checkout_id)
                                .cloned()
                                .with_context(|| {
                                    format!(
                                        "own checkout overlay is missing after one bounded refresh for scope {scope:?} and checkout {}",
                                        own.checkout_id
                                    )
                                })?
                        }
                    };
                    if snapshot.status != OverlayStatus::Valid {
                        anyhow::bail!(
                            "own checkout overlay is invalid for scope {scope:?}: {}",
                            snapshot.diagnostics.join("; ")
                        );
                    }
                    diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
                        format!(
                            "checkout {} in scope {scope:?}: {diagnostic}",
                            snapshot.key.checkout_id
                        )
                    }));
                    let overlay_ref =
                        intern_overlay_stamp(&mut built_from, &snapshot, &mut diagnostics);
                    apply_own_overlay(
                        &mut items,
                        &snapshot,
                        OverlayRowProject::LegacyPath(&project.canonical_path),
                        overlay_ref.as_deref(),
                    );
                }
                ProvisionalMode::All => {
                    let snapshots = self
                        .state
                        .knowledge_overlays
                        .read()
                        .snapshots()
                        .filter(|snapshot| snapshot.key.published_scope == scope)
                        .cloned()
                        .collect::<Vec<_>>();
                    for snapshot in snapshots {
                        if snapshot.status != OverlayStatus::Valid {
                            diagnostics.push(format!(
                                "checkout {} in scope {scope:?}: {}",
                                snapshot.key.checkout_id,
                                snapshot.diagnostics.join("; ")
                            ));
                            continue;
                        }
                        diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
                            format!(
                                "checkout {} in scope {scope:?}: {diagnostic}",
                                snapshot.key.checkout_id
                            )
                        }));
                        for (entry_id, value) in &snapshot.values {
                            if matches!(value, OverlayValue::Tombstone) {
                                diagnostics.push(format!(
                                    "checkout {} tombstones knowledge:{entry_id}",
                                    snapshot.key.checkout_id
                                ));
                            }
                        }
                        let overlay_ref =
                            intern_overlay_stamp(&mut built_from, &snapshot, &mut diagnostics);
                        add_overlay_upserts(
                            &mut items,
                            &snapshot,
                            OverlayRowProject::LegacyPath(&project.canonical_path),
                            overlay_ref.as_deref(),
                        );
                    }
                }
            }
        }

        if has_legacy_compatibility_rows {
            diagnostics.push(LEGACY_COMPATIBILITY_KNOWLEDGE_DIAGNOSTIC.into());
        }

        let items = items.into_values().collect::<Vec<_>>();
        built_from.retain_ids(
            items
                .iter()
                .filter_map(|item| item.metadata.built_from_ref.as_deref()),
        );
        let mut metadata = BTreeMap::new();
        let entries = items
            .iter()
            .map(|item| {
                let mut entry = item.entry.clone();
                if item.entity_ref.starts_with("provisional_knowledge:") {
                    entry.id = item.entity_ref.clone();
                }
                metadata.insert(entry.id.clone(), item.metadata.clone());
                entry
            })
            .collect();
        Ok(SessionKnowledgeView {
            knowledge: Knowledge::detached_view(entries, metadata),
            items,
            built_from,
            diagnostics,
            degraded_overlays,
        })
    }

    /// Serve accepted published knowledge for every selected catalog
    /// project. Nothing here can fail the whole view: a project whose
    /// publication is missing, corrupt, or serving its prior generation
    /// degrades to a bounded diagnostic while its peers keep serving.
    #[allow(clippy::too_many_arguments)] // one accumulator per view output
    fn append_catalog_published_knowledge(
        &self,
        requested_selector: Option<&str>,
        requested_project_id: Option<&str>,
        mode: ProvisionalMode,
        session_checkout: Option<&ResolvedCheckoutScope>,
        session_workspace: Option<&super::knowledge_source::WorkspaceBindingGrant>,
        items: &mut BTreeMap<String, KnowledgeViewItem>,
        built_from: &mut BuiltFromTable,
        diagnostics: &mut Vec<String>,
        degraded_overlays: &mut Vec<OverlayDegradation>,
    ) -> Result<()> {
        let Some(runtime) = self.state.accepted_publications.clone() else {
            diagnostics.push(
                "accepted-publication runtime is unavailable; no catalog published knowledge \
                 can be served"
                    .into(),
            );
            return Ok(());
        };
        if requested_selector.is_some() && requested_project_id.is_none() {
            // Filter-class semantics: an unresolved selector narrows
            // nothing. Say so rather than echoing the raw selector, which
            // may be an operator path.
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
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::PublishedKnowledge,
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                        );
                    }
                    diagnostics.push(catalog_publication_diagnostic(
                        target.project_id.as_str(),
                        &error,
                    ));
                    continue;
                }
            };
            self.observe_knowledge_transport_operation(
                target.project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::PublishedKnowledge,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
            );
            diagnostics.extend(catalog_publication_degradations(
                target.project_id.as_str(),
                &verified,
                target.catalog_scope.as_ref(),
            ));
            let published = self.cached_catalog_published_knowledge(&target.project_id, &verified);
            let published_scope = published.published_scope.clone();
            let published_ref = built_from.intern(BuiltFromStamp::Published {
                published_scope: published.published_scope,
                published_ref: published.published_ref,
                publisher_commit: published.publisher_commit,
            });
            for published_entry in published.entries.into_values() {
                insert_published_item(
                    items,
                    published_entry.entry,
                    Some(published_scope.clone()),
                    Some(published_entry.content_hash),
                    Some(&published_ref),
                    None,
                );
            }
            match mode {
                // Published ignores overlay failure entirely: accepted
                // content is authority and needs no checkout (D-007).
                ProvisionalMode::Published => {}
                ProvisionalMode::Own => self.apply_catalog_own_knowledge_overlay(
                    &target.project_id,
                    &verified,
                    session_checkout,
                    session_workspace,
                    items,
                    built_from,
                    diagnostics,
                )?,
                ProvisionalMode::All => self.append_catalog_all_knowledge_overlays(
                    &target.project_id,
                    &verified,
                    items,
                    built_from,
                    diagnostics,
                    degraded_overlays,
                )?,
            }
        }
        Ok(())
    }

    /// Apply the session checkout's own provisional layer, or refuse.
    ///
    /// `own` is the strict mode: the caller asked for one named checkout's
    /// view, and a checkout that cannot position itself against accepted
    /// content has no honest answer to give. Borrowing another
    /// attachment's ancestry to produce one is exactly what D-007 forbids,
    /// so the failure travels out with its exact underlying code.
    fn apply_catalog_own_knowledge_overlay(
        &self,
        project_id: &ProjectId,
        verified: &VerifiedAcceptedPublication,
        session_checkout: Option<&ResolvedCheckoutScope>,
        session_workspace: Option<&super::knowledge_source::WorkspaceBindingGrant>,
        items: &mut BTreeMap<String, KnowledgeViewItem>,
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
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
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
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                    );
                    anyhow::anyhow!("{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: {error:#}")
                })?
                .ok_or_else(|| {
                    self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                    );
                    anyhow::anyhow!(
                        "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: no live provisional generation is selected for the bound workspace"
                    )
                })?;
            if pair.knowledge.status != OverlayStatus::Valid {
                self.observe_knowledge_transport_operation(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                );
                anyhow::bail!(
                    "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: project {project_id} workspace {}: {}",
                    workspace.workspace_id,
                    pair.knowledge.diagnostics.join("; ")
                );
            }
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
            );
            if !transport_coverage.transport_governed() {
                let local = self
                    .catalog_overlay_attachment(project_id, workspace.workspace_id.as_str())
                    .and_then(|attachment| attachment.map_err(provisional_overlay_unavailable))
                    .and_then(|attachment| {
                        self.refresh_catalog_knowledge_overlay(verified, &attachment)
                            .map_err(provisional_overlay_unavailable)
                    });
                match local {
                    Ok(local) if local.status == OverlayStatus::Valid => {
                        self.observe_knowledge_transport_operation(
                            project_id.as_str(),
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
                        );
                        self.observe_knowledge_transport_shadow(
                            project_id.as_str(),
                            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                            Some(workspace.workspace_id.as_str()),
                            &local.snapshot_id,
                            &pair.knowledge.snapshot_id,
                        );
                    }
                    Ok(_) | Err(_) => self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                    ),
                }
            }
            diagnostics.extend(pair.knowledge.diagnostics.iter().map(|diagnostic| {
                format!(
                    "project {project_id} workspace {}: {diagnostic}",
                    workspace.workspace_id
                )
            }));
            let overlay_ref = intern_overlay_stamp(built_from, &pair.knowledge, diagnostics);
            apply_own_overlay(
                items,
                &pair.knowledge,
                OverlayRowProject::Catalog(project_id.as_str()),
                overlay_ref.as_deref(),
            );
            return Ok(());
        }
        if transport_coverage.transport_governed() {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            anyhow::bail!(
                "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: project {project_id} requires a live bound remote workspace"
            );
        }
        // A session checkout belongs to one project. Every other selected
        // project serves published rows only, exactly as the bridge does.
        let Some(own) = session_checkout.filter(|own| own.project_id == project_id.as_str()) else {
            return Ok(());
        };
        let attachment = self
            .catalog_overlay_attachment(project_id, &own.checkout_id)
            .context("selecting the attachment carrying the session checkout")?
            .map_err(provisional_overlay_unavailable)?;
        let snapshot = self
            .refresh_catalog_knowledge_overlay(verified, &attachment)
            .map_err(provisional_overlay_unavailable)?;
        if snapshot.status != OverlayStatus::Valid {
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
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
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
        );
        if let Some(workspace) = session_workspace.filter(|workspace| {
            workspace.project_id == project_id.as_str()
                && workspace.workspace_id.as_str() == own.checkout_id
        }) {
            match self.remote_provisional_overlays(workspace, verified) {
                Ok(Some(remote)) => self.observe_knowledge_transport_shadow(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                    Some(workspace.workspace_id.as_str()),
                    &snapshot.snapshot_id,
                    &remote.knowledge.snapshot_id,
                ),
                Ok(None) | Err(_) => self.observe_knowledge_transport_operation(
                    project_id.as_str(),
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalOwnKnowledge,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                ),
            }
        }
        diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
            format!(
                "project {project_id} checkout {}: {diagnostic}",
                snapshot.key.checkout_id
            )
        }));
        let overlay_ref = intern_overlay_stamp(built_from, &snapshot, diagnostics);
        apply_own_overlay(
            items,
            &snapshot,
            OverlayRowProject::Catalog(project_id.as_str()),
            overlay_ref.as_deref(),
        );
        Ok(())
    }

    /// Add every peer checkout's provisional upserts, omitting only the
    /// peers that failed and reporting each one.
    ///
    /// `all` is the survey mode: one unavailable peer is a fact about that
    /// peer, not about the answer, so accepted content and every healthy
    /// peer keep serving while the failures ride bounded degradation.
    fn append_catalog_all_knowledge_overlays(
        &self,
        project_id: &ProjectId,
        verified: &VerifiedAcceptedPublication,
        items: &mut BTreeMap<String, KnowledgeViewItem>,
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
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            diagnostics.push(format!(
                "project {project_id} provisional peers are unavailable pending knowledge transport re-cutover"
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
            let degraded = match self.refresh_catalog_knowledge_overlay(verified, &attachment) {
                Ok(snapshot) if snapshot.status == OverlayStatus::Valid => {
                    self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
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
                                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                                Some(workspace_id.as_str()),
                                &snapshot.snapshot_id,
                                &remote.knowledge.snapshot_id,
                            ),
                            Ok(None) | Err(_) => self.observe_knowledge_transport_operation(
                                project_id.as_str(),
                                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                            ),
                        }
                    }
                    add_catalog_overlay_rows(project_id, &snapshot, items, built_from, diagnostics);
                    continue;
                }
                Ok(snapshot) => {
                    OverlayDegradation::invalid_snapshot(project_id, &attachment, &snapshot)
                }
                Err(degradation) => degradation,
            };
            self.observe_knowledge_transport_operation(
                project_id.as_str(),
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            // The peer is omitted, never faked: its reason rides both the
            // structured report and the human diagnostics.
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
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                    bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
                );
                diagnostics.push(format!(
                    "project {project_id} remote provisional workspace inventory is unavailable: {error:#}"
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
                Ok(Some(pair)) if pair.knowledge.status == OverlayStatus::Valid => {
                    self.observe_knowledge_transport_operation(
                        project_id.as_str(),
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                        bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
                    );
                    add_catalog_overlay_rows(
                        project_id,
                        &pair.knowledge,
                        items,
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
                    detail: pair.knowledge.diagnostics.join("; "),
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
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProvisionalAllKnowledge,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Degraded,
            );
            diagnostics.push(degraded.diagnostic_line());
            degraded_overlays.push(degraded);
        }
        Ok(())
    }

    /// Every active attachment that may position its checkout against one
    /// project's accepted content, in durable workspace-id order. Strict
    /// transport reconstructs `all` from workspace ids after attachments are
    /// gone, so overlap must compose peers in that same order.
    ///
    /// Capability is deliberately not filtered here. `all` must report a
    /// capability-denied peer rather than quietly drop it, so the broker
    /// stays the only thing that answers whether an attachment may open a
    /// checkout (plan section 9).
    pub(crate) fn catalog_active_overlay_attachments(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<CatalogOverlayAttachment>> {
        let mut attachments = self
            .catalog_project_attachments(project_id)?
            .into_iter()
            .filter(|(_, status)| *status == AttachmentStatus::Attached)
            .map(|(attachment, _)| attachment)
            .collect::<Vec<_>>();
        attachments.sort_by(|left, right| {
            left.checkout_id
                .cmp(&right.checkout_id)
                .then_with(|| left.attachment_id.cmp(&right.attachment_id))
        });
        Ok(attachments)
    }

    /// The attachment carrying one checkout, or the exact reason there is
    /// none. `own` never falls back to another attachment: a checkout that
    /// is gone or detached has no ancestry to lend it (D-007).
    pub(crate) fn catalog_overlay_attachment(
        &self,
        project_id: &ProjectId,
        checkout_id: &str,
    ) -> Result<std::result::Result<CatalogOverlayAttachment, OverlayDegradation>> {
        let found = self
            .catalog_project_attachments(project_id)?
            .into_iter()
            .find(|(attachment, _)| attachment.checkout_id == checkout_id);
        Ok(match found {
            Some((attachment, AttachmentStatus::Attached)) => Ok(attachment),
            Some((attachment, _)) => Err(OverlayDegradation {
                project_id: project_id.as_str().to_string(),
                checkout_id: checkout_id.to_string(),
                attachment_id: Some(attachment.attachment_id),
                code: CheckoutAccessErrorCode::AttachmentInactive.as_str(),
                detail: "the attachment carrying this checkout is detached".into(),
                transient: false,
            }),
            None => Err(OverlayDegradation {
                project_id: project_id.as_str().to_string(),
                checkout_id: checkout_id.to_string(),
                attachment_id: None,
                code: CheckoutAccessErrorCode::AttachmentNotFound.as_str(),
                detail: "no attachment carries this checkout".into(),
                transient: false,
            }),
        })
    }

    fn catalog_project_attachments(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<(CatalogOverlayAttachment, AttachmentStatus)>> {
        let Some(store) = self.state.project_authority.catalog_store() else {
            return Ok(Vec::new());
        };
        let state = store.snapshot().map_err(anyhow::Error::new)?;
        Ok(state
            .attachments()
            .attachments
            .values()
            .filter(|row| &row.project_id == project_id)
            .map(|row| {
                (
                    CatalogOverlayAttachment {
                        attachment_id: row.attachment_id.as_str().to_string(),
                        checkout_id: row.checkout_id.clone(),
                    },
                    row.status,
                )
            })
            .collect())
    }

    /// Recompute one catalog checkout's provisional knowledge overlay
    /// against verified accepted content.
    ///
    /// This lives beside its caller rather than next to the bridge refresh
    /// in `src/tools/knowledge.rs`. That refresh is publisher-election
    /// surface headed for the Phase 6 deletion inventory, and the two
    /// paths share no authority: the bridge elects a publisher repository
    /// and reads ancestry through it, while a catalog overlay takes
    /// content from accepted bytes and ancestry from this checkout alone
    /// (D-007). The file split is the intended end state, not debt.
    pub(crate) fn refresh_catalog_knowledge_overlay(
        &self,
        verified: &VerifiedAcceptedPublication,
        attachment: &CatalogOverlayAttachment,
    ) -> std::result::Result<OverlaySnapshot, OverlayDegradation> {
        let _refresh = self.state.knowledge_overlay_refresh.lock();
        let content_stamp = verified.content_stamp();
        let scope = content_stamp.accepted_scope().clone();
        let key = OverlayKey {
            published_scope: scope.clone(),
            checkout_id: attachment.checkout_id.clone(),
        };

        let generation = self
            .state
            .knowledge_overlays
            .write()
            .begin_refresh(key.clone());
        let prior = self
            .state
            .knowledge_overlays
            .read()
            .get(&scope, &attachment.checkout_id)
            .cloned();
        let prior_is_valid = prior
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == OverlayStatus::Valid);

        let failure =
            match self.compute_catalog_knowledge_overlay(verified, attachment, &scope, || {}) {
                Ok(snapshot) => {
                    // The refresh mutex is held across the whole sequence, so
                    // no newer generation can exist here; publication is the
                    // observability record, and the caller is served the
                    // snapshot it just computed either way.
                    self.state
                        .knowledge_overlays
                        .write()
                        .publish_if_latest(generation, snapshot.clone());
                    return Ok(snapshot);
                }
                Err(failure) => failure,
            };

        // Bounded preservation for transient failures only. A structural
        // failure carries `transient = false` by construction, so a
        // detached publisher, a missing accepted commit, or an absent
        // merge base can never be masked by a stale valid snapshot
        // (plan section 4.12).
        if failure.transient && prior_is_valid {
            let mut preserved = prior.expect("prior valid snapshot");
            preserved.diagnostics = vec![failure.diagnostic_line()];
            match self
                .state
                .knowledge_overlays
                .write()
                .preserve_transient_if_latest(generation, preserved.clone())
            {
                TransientPreservationOutcome::Preserved { .. }
                | TransientPreservationOutcome::Superseded => return Ok(preserved),
                TransientPreservationOutcome::Exhausted => {}
            }
        }
        self.state.knowledge_overlays.write().publish_if_latest(
            generation,
            invalid_knowledge_overlay(&key, failure.diagnostic_line()),
        );
        Err(failure)
    }

    /// One checkout positioned against accepted content, with the lease and
    /// the accepted identity both proved after the capture.
    ///
    /// `after_capture` runs inside the capture window. Production passes a
    /// no-op; it is the seam that lets a test move the checkout, detach the
    /// attachment, or advance accepted content at the exact point these
    /// proofs exist to catch.
    fn compute_catalog_knowledge_overlay(
        &self,
        verified: &VerifiedAcceptedPublication,
        attachment: &CatalogOverlayAttachment,
        scope: &PublishedScope,
        after_capture: impl FnMut(),
    ) -> std::result::Result<OverlaySnapshot, OverlayDegradation> {
        let content_stamp = verified.content_stamp();
        let project_id = content_stamp.project_id();
        let published = accepted_knowledge_digests(verified);
        let lease = self.acquire_catalog_overlay_lease(project_id, attachment, scope)?;
        let snapshot = stable_catalog_knowledge_overlay(
            CatalogOverlayPublished {
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
        .map_err(|error| {
            OverlayDegradation::from_knowledge_recompute(project_id, attachment, &error)
        })?;
        // Both identities are proved after the capture and before the
        // snapshot is published (plan section 8, P5-D mechanics step 10):
        // a detach makes the bytes unauthorized, and an advance makes them
        // a position against content that is no longer published.
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

    /// Acquire one native `KnowledgeGapOverlayRead` lease.
    ///
    /// The selector is the attachment id, so the observation rides the
    /// native lane rather than a compatibility one. `expected_scope` is the
    /// ACCEPTED scope, not the catalog's current scope: the diff is defined
    /// under the accepted scope's knowledge directory, so an attachment
    /// validated at a migrated scope must refuse rather than diff the wrong
    /// tree (plan section 4.9).
    pub(crate) fn acquire_catalog_overlay_lease(
        &self,
        project_id: &ProjectId,
        attachment: &CatalogOverlayAttachment,
        scope: &PublishedScope,
    ) -> std::result::Result<ValidatedCheckoutLease, OverlayDegradation> {
        self.state
            .checkout_access
            .acquire(CheckoutAccessRequest {
                project_id: project_id.as_str().to_string(),
                attachment: CheckoutAttachmentSelector::AttachmentId(
                    attachment.attachment_id.clone(),
                ),
                expected_scope: Some(scope.clone()),
                kind: CheckoutAccessKind::KnowledgeGapOverlayRead,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::NativeAttachment,
            })
            .map_err(|error| {
                OverlayDegradation::from_checkout_access(project_id, Some(attachment), &error)
            })
    }

    /// True while the pointer still names the accepted content that stamped
    /// an overlay. An advance during capture invalidates the snapshot.
    pub(crate) fn catalog_accepted_content_unchanged(
        &self,
        content_stamp: &AcceptedPublicationContentStamp,
    ) -> bool {
        self.state
            .accepted_publications
            .as_ref()
            .is_some_and(|runtime| {
                runtime
                    .load_verified(content_stamp.project_id())
                    .is_ok_and(|current| current.content_stamp() == content_stamp)
            })
    }

    /// Project accepted records once per accepted content identity. The
    /// content stamp is the validity token: a rebind leaves it unchanged and
    /// keeps this entry, while an advance replaces it.
    fn cached_catalog_published_knowledge(
        &self,
        project_id: &ProjectId,
        verified: &VerifiedAcceptedPublication,
    ) -> PublishedKnowledgeSnapshot {
        let content_stamp = verified.content_stamp();
        let cached = self
            .state
            .catalog_knowledge_published_cache
            .read()
            .get(project_id)
            .filter(|entry| &entry.content_stamp == content_stamp)
            .map(|entry| entry.snapshot.clone());
        if let Some(cached) = cached {
            return cached;
        }
        let snapshot = published_knowledge_from_accepted(verified);
        self.state.catalog_knowledge_published_cache.write().insert(
            project_id.clone(),
            CatalogPublishedKnowledgeCacheEntry {
                content_stamp: content_stamp.clone(),
                snapshot: snapshot.clone(),
            },
        );
        snapshot
    }

    /// Reconcile the published knowledge index from durable accepted
    /// content for every catalog project, at startup.
    ///
    /// Live convergence is an asynchronous enqueue with no durable record,
    /// so a process that dies between the pointer swap and the index
    /// commit would otherwise serve the new generation from accepted reads
    /// and the old one from search, forever. This pass closes that window
    /// by reprojecting from the pointer, which is the durable authority.
    ///
    /// It is deliberately stateless: no convergence obligation is
    /// persisted at swap time, and no replay log has to be recovered.
    /// Cost is one reprojection per published project per boot, bounded by
    /// the catalog.
    pub(crate) fn converge_published_knowledge_at_startup(&self) -> StartupConvergenceReport {
        let mut report = StartupConvergenceReport::default();
        if self.state.accepted_publications.is_none() {
            return report;
        }
        let targets = match self.catalog_published_targets(None) {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "startup published-index convergence could not read the catalog"
                );
                return report;
            }
        };
        for target in targets {
            report.visited += 1;
            if self.converge_published_knowledge_index(&target.project_id) {
                report.converged += 1;
            } else {
                report.skipped += 1;
            }
        }
        report
    }

    /// Reconverge the published knowledge index for one catalog project
    /// after its accepted content moved (plan section 7.3 step 19).
    ///
    /// The convergence is bounded: one scope replacement built from the
    /// project's own view, enqueued on the single index writer. Failure is
    /// degradation, not corruption, so it warns rather than propagating:
    /// the pointer and the projected caches are already correct, and the
    /// next reindex pass reconciles the search index.
    ///
    /// The gap lane has no counterpart on purpose. Gaps are not tantivy
    /// documents; `session_gap_view` reads them live from accepted content
    /// through the projection caches, so invalidating those caches IS the
    /// gap lane's convergence and there is no index to replace.
    pub(crate) fn converge_published_knowledge_index(&self, project_id: &ProjectId) -> bool {
        let Some(runtime) = &self.state.accepted_publications else {
            return false;
        };
        let scope = match runtime.load_verified(project_id) {
            Ok(verified) => verified.content_stamp().accepted_scope().clone(),
            Err(error) => {
                // No verified content to converge to. A project whose
                // publication is missing or corrupt keeps whatever the
                // index already holds; clearing it here would delete rows
                // a Prior fallback may still be serving.
                tracing::warn!(
                    project_id = %project_id,
                    code = error.code(),
                    "published index convergence skipped: no verified accepted content"
                );
                return false;
            }
        };
        if let Err(error) = self.sync_knowledge_scope_to_index(&scope, project_id.as_str()) {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "published index convergence failed; the next reindex pass reconciles it"
            );
            return false;
        }
        true
    }

    /// Drop every catalog-side cache derived from one project's accepted
    /// content. Advance calls this; rebind must not, because a binding
    /// change leaves accepted content identical.
    #[allow(dead_code)] // P5-B installs the invalidator; P5-C advance calls it.
    pub(crate) fn invalidate_catalog_published_content(&self, project_id: &ProjectId) {
        if let Some(runtime) = &self.state.accepted_publications {
            runtime.invalidate_content(project_id);
        }
        self.state
            .catalog_knowledge_published_cache
            .write()
            .remove(project_id);
        self.state
            .catalog_gap_published_cache
            .write()
            .remove(project_id);
    }

    /// Rebuild the published project-graph view for one catalog project
    /// after its accepted content moved.
    ///
    /// Unlike knowledge and gaps, the graph read surface
    /// (`project_graph_views`) has no lazy rebuild-on-read path: reads
    /// serve whatever was last installed. Advance must actively refresh it
    /// here, or a project's graphs stay invisible (or stale) after an
    /// accept until something unrelated, such as binding a checkout,
    /// happens to install a fresh view. Failure degrades rather than
    /// propagates, matching `converge_published_knowledge_index`: the
    /// pointer is already correct, and the next accept reconciles the view.
    pub(crate) fn refresh_published_graph_views(&self, project_id: &ProjectId) {
        let Some(runtime) = &self.state.accepted_publications else {
            return;
        };
        let verified = match runtime.load_verified(project_id) {
            Ok(verified) => verified,
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    code = error.code(),
                    "published graph view refresh skipped: no verified accepted content"
                );
                return;
            }
        };
        match bbox_indexing::project_graph_view::build_published_graph_view(&verified) {
            Ok(view) => {
                install_published_graph_view(&self.state, view);
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "published graph view refresh failed; graph reads may serve stale content \
                     until the next accept"
                );
            }
        }
    }

    fn cached_published_knowledge_snapshot(
        &self,
        publisher: &super::knowledge_lifecycle::AuthorizedPublisher,
        scope: &PublishedScope,
        durable_project: &str,
    ) -> Result<PublishedKnowledgeSnapshot> {
        let cached = self
            .state
            .knowledge_published_cache
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
            let mut snapshot = cached.snapshot.clone();
            self.with_authorized_publisher_root(publisher, |root| {
                hydrate_published_snapshot(root, &mut snapshot);
                Ok(())
            })?;
            return Ok(snapshot);
        }

        let snapshot = self.with_authorized_publisher_root(publisher, |root| {
            load_published_snapshot_at_commit_unhydrated(
                root,
                &publisher.branch_ref,
                &publisher.commit,
                scope,
                durable_project,
            )
        })?;
        self.state.knowledge_published_cache.write().insert(
            scope.clone(),
            PublishedKnowledgeCacheEntry {
                publisher_project_id: publisher.project_id.clone(),
                publisher_commit: publisher.commit.clone(),
                durable_project: durable_project.to_string(),
                snapshot: snapshot.clone(),
            },
        );
        let mut hydrated = snapshot;
        self.with_authorized_publisher_root(publisher, |root| {
            hydrate_published_snapshot(root, &mut hydrated);
            Ok(())
        })?;
        Ok(hydrated)
    }
}

/// Install one published project-graph view and converge the project's
/// published graph word lanes to it (unified-retrieval design 7.1).
///
/// Delegates to [`converge_published_graph_word_lanes`] for the durable side,
/// then swaps the catalog view under its own write guard. Callers that also
/// install a provisional overlay in the same step must use
/// [`converge_published_graph_word_lanes`] plus one shared write guard
/// instead, so a reader can never observe the new published view with the old
/// (or missing) provisional overlay.
pub(crate) fn install_published_graph_view(
    state: &SharedState,
    view: bbox_indexing::project_graph_view::PublishedProjectGraphView,
) {
    converge_published_graph_word_lanes(state, &view);
    state.project_graph_views.write().install_published(view);
}

/// Converge the published graph word lanes to one view WITHOUT touching the
/// in-memory catalog: the lane replacements and purges are computed and
/// enqueued here, and the caller performs the catalog swap under whatever
/// guard it needs (alone, or shared with a provisional install).
///
/// Every graph in the view gets a whole-lane replacement keyed on its
/// generation stamp: same generation no-ops, a new generation rewrites the
/// lane, and a graph whose policy now disables text retrieval (or that left
/// the accepted view entirely) has its lane purged so its documents are
/// ABSENT from the index, not merely filtered out of one result list. M9a
/// indexes the published plane only; provisional and connector planes do not
/// reach the word index yet and must not piggyback on this path.
pub(crate) fn converge_published_graph_word_lanes(
    state: &SharedState,
    view: &bbox_indexing::project_graph_view::PublishedProjectGraphView,
) {
    use bbox_indexing::index::{
        GRAPH_SOURCE_PUBLISHED as PUBLISHED, published_graph_vertex_documents,
    };

    let project_id = view.project_id.as_str().to_string();
    let indexed_lanes = state
        .idx
        .read()
        .graph_lanes_for_project(&project_id, PUBLISHED)
        .unwrap_or_else(|error| {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "published graph word lane inventory failed during view install"
            );
            BTreeMap::new()
        });
    let mut planned = BTreeSet::new();
    for (graph_id, entry) in &view.graphs {
        planned.insert(graph_id.clone());
        let Some(graph) = entry.graph() else {
            state
                .index_writer
                .purge_project_graph_lane(&project_id, graph_id, PUBLISHED);
            continue;
        };
        let documents =
            published_graph_vertex_documents(&project_id, graph, &entry.generation.content_hash);
        state.index_writer.replace_project_graph_lane(
            &project_id,
            graph_id,
            PUBLISHED,
            &entry.generation.content_hash,
            documents,
        );
    }
    for graph_id in indexed_lanes.keys() {
        if !planned.contains(graph_id) {
            state
                .index_writer
                .purge_project_graph_lane(&project_id, graph_id, PUBLISHED);
        }
    }
}

// ── Catalog overlay baseline path (plan section 8, P5-D) ─────────────────

/// `own` refuses with this code and carries the exact underlying overlay or
/// checkout code inside it (plan sections 10.4 and 10.5).
pub(crate) const ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE: &str =
    "error.provisional_overlay_unavailable";
/// The checkout cannot prove the baseline: it does not contain the accepted
/// commit, or it shares no merge base with it. Structural, never transient.
pub(crate) const ERROR_OVERLAY_BASELINE_UNAVAILABLE: &str = "error.overlay_baseline_unavailable";
/// No current snapshot exists for this checkout. The overlay vocabulary has
/// exactly one non-structural failure code, and both causes say the same
/// thing to a caller: invalid working content, and a checkout that never
/// settled across a bounded capture.
pub(crate) const ERROR_OVERLAY_SNAPSHOT_STALE: &str = "error.overlay_snapshot_stale";
/// Accepted content advanced between the capture and the publication, so the
/// snapshot positions the checkout against bytes that are no longer published.
pub(crate) const ERROR_OVERLAY_ACCEPTED_CONTENT_CHANGED: &str =
    "error.overlay_accepted_content_changed";

/// One attachment that may position its checkout against accepted content.
/// Identity only: an overlay carrier never holds a host path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogOverlayAttachment {
    pub(crate) attachment_id: String,
    pub(crate) checkout_id: String,
}

/// One checkout that could not position itself against accepted content.
///
/// Every field is bounded identity or a stable code. The underlying error
/// text can name absolute paths and raw Git output, so it goes to the log
/// and an authored sentence goes to the response (plan section 10.5).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct OverlayDegradation {
    pub(crate) project_id: String,
    pub(crate) checkout_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attachment_id: Option<String>,
    pub(crate) code: &'static str,
    pub(crate) detail: String,
    /// Whether a bounded transient window may preserve a prior valid
    /// snapshot over this failure. Structural authority facts may not.
    #[serde(skip)]
    pub(crate) transient: bool,
}

impl OverlayDegradation {
    pub(crate) fn diagnostic_line(&self) -> String {
        match &self.attachment_id {
            Some(attachment_id) => format!(
                "project {} checkout {} (attachment {attachment_id}): {}: {}",
                self.project_id, self.checkout_id, self.code, self.detail
            ),
            None => format!(
                "project {} checkout {}: {}: {}",
                self.project_id, self.checkout_id, self.code, self.detail
            ),
        }
    }

    /// Classify a checkout-access refusal into the plan's degradation
    /// vocabulary (plan section 10.2). Only `lifecycle_busy` and a
    /// momentarily unreadable observation store are retryable; every other
    /// refusal is a fact about authority that a stale snapshot must not hide.
    pub(crate) fn from_checkout_access(
        project_id: &ProjectId,
        attachment: Option<&CatalogOverlayAttachment>,
        error: &CheckoutAccessError,
    ) -> Self {
        let detail = match error.code {
            CheckoutAccessErrorCode::AttachmentNotFound => "no active attachment is available",
            CheckoutAccessErrorCode::AttachmentInactive => "the attachment is detached",
            CheckoutAccessErrorCode::CapabilityDenied => {
                "the attachment does not record the repo-knowledge capability"
            }
            CheckoutAccessErrorCode::LifecycleBusy => "the checkout lifecycle is busy",
            CheckoutAccessErrorCode::CheckoutIdentityMismatch => {
                "the checkout no longer proves its recorded identity"
            }
            CheckoutAccessErrorCode::InvalidRoot
            | CheckoutAccessErrorCode::UnsafeRelativePath
            | CheckoutAccessErrorCode::ConservativePathGateDenied => {
                "the checkout root is unsafe or no longer valid"
            }
            CheckoutAccessErrorCode::ScopeMismatch => {
                "the attachment is validated at a different published scope"
            }
            _ => "the checkout could not be leased for an overlay read",
        };
        tracing::debug!(
            project_id = %project_id,
            code = error.code.as_str(),
            error = %error,
            "catalog overlay checkout access refused"
        );
        Self {
            project_id: project_id.as_str().to_string(),
            checkout_id: attachment
                .map(|attachment| attachment.checkout_id.clone())
                .unwrap_or_default(),
            attachment_id: attachment.map(|attachment| attachment.attachment_id.clone()),
            code: error.code.as_str(),
            detail: detail.to_string(),
            transient: matches!(
                error.code,
                CheckoutAccessErrorCode::LifecycleBusy
                    | CheckoutAccessErrorCode::ObservationUnavailable
            ),
        }
    }

    fn from_knowledge_recompute(
        project_id: &ProjectId,
        attachment: &CatalogOverlayAttachment,
        error: &OverlayRecomputeError,
    ) -> Self {
        let (code, detail, transient) = match error.kind {
            OverlayRecomputeErrorKind::BaselineUnavailable => (
                ERROR_OVERLAY_BASELINE_UNAVAILABLE,
                "the checkout does not contain the accepted commit or shares no merge base with it",
                false,
            ),
            OverlayRecomputeErrorKind::InvalidContent => (
                ERROR_OVERLAY_SNAPSHOT_STALE,
                "the checkout's knowledge files are not valid published content",
                false,
            ),
            OverlayRecomputeErrorKind::Transient => (
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
            "catalog knowledge overlay recompute failed"
        );
        Self {
            project_id: project_id.as_str().to_string(),
            checkout_id: attachment.checkout_id.clone(),
            attachment_id: Some(attachment.attachment_id.clone()),
            code,
            detail: detail.to_string(),
            transient,
        }
    }

    /// A published-but-invalid snapshot: the peer has a store entry and no
    /// usable values, which `all` reports rather than serving.
    fn invalid_snapshot(
        project_id: &ProjectId,
        attachment: &CatalogOverlayAttachment,
        snapshot: &OverlaySnapshot,
    ) -> Self {
        Self {
            project_id: project_id.as_str().to_string(),
            checkout_id: attachment.checkout_id.clone(),
            attachment_id: Some(attachment.attachment_id.clone()),
            code: ERROR_OVERLAY_SNAPSHOT_STALE,
            detail: snapshot.diagnostics.join("; "),
            transient: false,
        }
    }
}

/// Wrap one degradation in the mode's stable code. `own` returns a tool
/// error, so the exact underlying code has to survive into the text.
pub(super) fn provisional_overlay_unavailable(degradation: OverlayDegradation) -> anyhow::Error {
    anyhow::anyhow!(
        "{ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE}: {}",
        degradation.diagnostic_line()
    )
}

/// An empty invalid snapshot for one overlay key.
///
/// The bridge builds this from a `ResolvedCheckoutScope`; a catalog refresh
/// has no such compatibility carrier and builds it from the key it already
/// reserved.
fn invalid_knowledge_overlay(key: &OverlayKey, diagnostic: String) -> OverlaySnapshot {
    OverlaySnapshot {
        snapshot_id: String::new(),
        key: key.clone(),
        stamp: None,
        status: OverlayStatus::Invalid,
        values: BTreeMap::new(),
        diagnostics: vec![diagnostic],
    }
}

/// Capture one overlay the checkout agrees with twice in a row.
///
/// Head, merge base, and working fingerprint all ride the snapshot id, so
/// one comparison covers every kind of movement during the capture. A
/// checkout that never settles inside the bounded window is transient: it
/// is busy, not wrong.
///
/// `after_capture` runs between the reads. Production passes a no-op; it is
/// the seam that lets a test move the checkout at the exact point the
/// stability discipline exists to catch.
fn stable_catalog_knowledge_overlay(
    published: CatalogOverlayPublished<'_>,
    lease: &ValidatedCheckoutLease,
    mut after_capture: impl FnMut(),
) -> std::result::Result<OverlaySnapshot, OverlayRecomputeError> {
    let pending = || {
        lease
            .checkout_relative_regular_file_exists(
                ".bbox/local/knowledge-transactions/pending.json",
            )
            .map_err(anyhow::Error::new)
            .map_err(OverlayRecomputeError::transient)
    };
    let working = || {
        let files = lease
            .read_relative_json_directory(".bbox/knowledge")
            .map_err(anyhow::Error::new)
            .map_err(OverlayRecomputeError::transient)?;
        WorkingKnowledgeSnapshot::new(files).map_err(OverlayRecomputeError::transient)
    };
    if pending()? {
        return Err(OverlayRecomputeError::transient(anyhow::anyhow!(
            "checkout transaction is pending; catalog overlay refresh deferred"
        )));
    }
    let first_working = working()?;
    let mut candidate =
        recompute_catalog_overlay_result(published, lease.checkout_root(), &first_working)?;
    for _ in 0..2 {
        after_capture();
        if pending()? {
            return Err(OverlayRecomputeError::transient(anyhow::anyhow!(
                "checkout transaction began during catalog overlay refresh"
            )));
        }
        let next_working = working()?;
        let next =
            recompute_catalog_overlay_result(published, lease.checkout_root(), &next_working)?;
        if same_knowledge_snapshot(&candidate, &next) && !pending()? {
            return Ok(next);
        }
        candidate = next;
    }
    Err(OverlayRecomputeError::transient(anyhow::anyhow!(
        "checkout state changed repeatedly during catalog overlay refresh"
    )))
}

fn same_knowledge_snapshot(left: &OverlaySnapshot, right: &OverlaySnapshot) -> bool {
    left.snapshot_id == right.snapshot_id
        && left.status == right.status
        && left.diagnostics == right.diagnostics
}

/// Project the accepted knowledge manifest into the identity the diff asks
/// for: does this file exist in published content, and do the working bytes
/// already equal it.
///
/// Manifest keys are repository-relative and the diff compares basenames
/// inside one published scope's knowledge directory. Basenames are unique
/// there by construction, because every manifest entry names a file in that
/// one directory.
pub(crate) fn accepted_knowledge_digests(
    verified: &VerifiedAcceptedPublication,
) -> AcceptedPublishedDigests {
    AcceptedPublishedDigests(
        verified
            .knowledge_manifest()
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

pub(crate) fn basename(repository_relative: &str) -> Option<String> {
    Path::new(repository_relative)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

/// Merge one valid peer snapshot's provisional upserts into the view.
fn add_catalog_overlay_rows(
    project_id: &ProjectId,
    snapshot: &OverlaySnapshot,
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    built_from: &mut BuiltFromTable,
    diagnostics: &mut Vec<String>,
) {
    diagnostics.extend(snapshot.diagnostics.iter().map(|diagnostic| {
        format!(
            "project {project_id} checkout {}: {diagnostic}",
            snapshot.key.checkout_id
        )
    }));
    // A peer's tombstone is a diagnostic, never a deletion: `all` surveys
    // checkouts, and one peer may not retract another's published rows.
    for (entry_id, value) in &snapshot.values {
        if matches!(value, OverlayValue::Tombstone) {
            diagnostics.push(format!(
                "checkout {} tombstones knowledge:{entry_id}",
                snapshot.key.checkout_id
            ));
        }
    }
    let overlay_ref = intern_overlay_stamp(built_from, snapshot, diagnostics);
    add_overlay_upserts(
        items,
        snapshot,
        OverlayRowProject::Catalog(project_id.as_str()),
        overlay_ref.as_deref(),
    );
}

/// Why one catalog project cannot serve published content. Only the stable
/// code crosses into a response: store detail can name store paths, and a
/// diagnostic must not.
pub(crate) fn catalog_publication_diagnostic(
    project_id: &str,
    error: &AcceptedPublicationRuntimeError,
) -> String {
    if error.code() == ERROR_ACCEPTED_PUBLICATION_MISSING {
        return format!(
            "project {project_id}: no accepted publication pointer, so published content is \
             unavailable"
        );
    }
    format!(
        "project {project_id}: accepted publication is unavailable ({})",
        error.code()
    )
}

/// Degradations that still serve content: the prior-generation fallback and
/// the scope-migration bridge. Both are read-only states the operator
/// repairs through the publisher surface.
pub(crate) fn catalog_publication_degradations(
    project_id: &str,
    verified: &VerifiedAcceptedPublication,
    catalog_scope: Option<&PublishedScope>,
) -> Vec<String> {
    let mut degradations = Vec::new();
    if verified.binding_stamp().selection() == AcceptedPublicationSelection::Prior {
        degradations.push(format!(
            "project {project_id}: the current accepted generation did not verify, so reads are \
             served from the prior generation and publisher mutation refuses until repair"
        ));
    }
    if verified.binding_stamp().scope_agreement(catalog_scope)
        == AcceptedPublicationScopeAgreement::RefreshRequired
    {
        degradations.push(format!(
            "project {project_id}: accepted content predates the catalog's current published \
             scope; it keeps its accepted scope until a new-scope advance"
        ));
    }
    degradations
}

/// Project one verified accepted generation into the published snapshot the
/// view layer already consumes.
///
/// The manifest is the authoritative file list, and its
/// `source_content_sha256` is the digest of the exact committed bytes, so a
/// catalog row carries the same content hash the publisher-root read would
/// have produced for the same commit.
fn published_knowledge_from_accepted(
    verified: &VerifiedAcceptedPublication,
) -> PublishedKnowledgeSnapshot {
    let content_stamp = verified.content_stamp();
    let mut entries = BTreeMap::new();
    for manifest in verified.knowledge_manifest().values() {
        // Generation validation makes the manifest and the normalized
        // records a bijection, so a miss here is unreachable rather than a
        // silently dropped row.
        let Some(record) = verified.knowledge_records().get(&manifest.record_id) else {
            continue;
        };
        let entry = knowledge_entry_from_accepted(record, content_stamp.project_id());
        entries.insert(
            entry.id.clone(),
            PublishedKnowledgeEntry {
                entry,
                content_hash: manifest.source_content_sha256.as_str().to_string(),
            },
        );
    }
    PublishedKnowledgeSnapshot {
        published_scope: content_stamp.accepted_scope().clone(),
        published_ref: content_stamp.full_ref().to_string(),
        publisher_commit: content_stamp.accepted_commit().to_string(),
        entries,
    }
}

/// Rebuild the domain entry from its accepted record.
///
/// The host-local fields accepted normalization dropped stay dropped.
/// `project` is a checkout path and a catalog read has no checkout, so
/// identity travels in `project_id`. Recall telemetry stays zero: it is
/// advisory, repo-local, and not part of accepted durable truth, and
/// restoring it would mean opening a checkout for a remote-only read
/// (plan section 4.14).
fn knowledge_entry_from_accepted(
    record: &AcceptedKnowledgeEntryV1,
    project_id: &ProjectId,
) -> KnowledgeEntry {
    KnowledgeEntry {
        id: record.id.as_str().to_string(),
        title: record.title.clone(),
        content: record.content.clone(),
        cluster: record.cluster.clone(),
        variants: record
            .variants
            .iter()
            .map(|(provider, content)| (provider.clone(), content.clone()))
            .collect(),
        category: match record.category {
            AcceptedKnowledgeCategoryV1::Profile => Category::Profile,
            AcceptedKnowledgeCategoryV1::Convention => Category::Convention,
            AcceptedKnowledgeCategoryV1::Steering => Category::Steering,
            AcceptedKnowledgeCategoryV1::Build => Category::Build,
            AcceptedKnowledgeCategoryV1::Tool => Category::Tool,
            AcceptedKnowledgeCategoryV1::Memory => Category::Memory,
            AcceptedKnowledgeCategoryV1::Workflow => Category::Workflow,
            AcceptedKnowledgeCategoryV1::Decision => Category::Decision,
        },
        // An accepted project generation cannot contain global knowledge:
        // normalization refuses it.
        scope: Scope::Project,
        project: None,
        project_id: Some(project_id.as_str().to_string()),
        providers: record.providers.clone(),
        priority: match record.priority {
            AcceptedKnowledgePriorityV1::Critical => Priority::Critical,
            AcceptedKnowledgePriorityV1::Standard => Priority::Standard,
            AcceptedKnowledgePriorityV1::Supplementary => Priority::Supplementary,
        },
        weight: record.weight,
        status: match record.status {
            AcceptedKnowledgeStatusV1::Active => Status::Active,
            AcceptedKnowledgeStatusV1::Draft => Status::Draft,
            AcceptedKnowledgeStatusV1::Superseded => Status::Superseded,
            AcceptedKnowledgeStatusV1::Disabled => Status::Disabled,
            AcceptedKnowledgeStatusV1::Deleted => Status::Deleted,
        },
        approval: match record.approval {
            AcceptedKnowledgeApprovalV1::UserConfirmed => Approval::UserConfirmed,
            AcceptedKnowledgeApprovalV1::AgentInferred => Approval::AgentInferred,
            AcceptedKnowledgeApprovalV1::Imported => Approval::Imported,
        },
        render: record.render,
        decay: record.decay,
        review_at: record.review_at.clone(),
        supersedes: record.supersedes.clone(),
        links: record
            .links
            .iter()
            .map(|edge| KnowledgeEdge {
                target: edge.target.clone(),
                kind: match edge.kind {
                    AcceptedKnowledgeEdgeKindV1::Contradicts => KnowledgeEdgeKind::Contradicts,
                    AcceptedKnowledgeEdgeKindV1::RelatesTo => KnowledgeEdgeKind::RelatesTo,
                    AcceptedKnowledgeEdgeKindV1::TensionWith => KnowledgeEdgeKind::TensionWith,
                    AcceptedKnowledgeEdgeKindV1::Supports => KnowledgeEdgeKind::Supports,
                    AcceptedKnowledgeEdgeKindV1::DependsOn => KnowledgeEdgeKind::DependsOn,
                    AcceptedKnowledgeEdgeKindV1::DerivedFrom => KnowledgeEdgeKind::DerivedFrom,
                    AcceptedKnowledgeEdgeKindV1::Supersedes => KnowledgeEdgeKind::Supersedes,
                    AcceptedKnowledgeEdgeKindV1::References => KnowledgeEdgeKind::References,
                },
                note: edge.note.clone(),
                source_arc: edge.source_arc.clone(),
                confidence: match edge.confidence {
                    AcceptedEdgeConfidenceV1::Exact => bbox_chunker::EdgeConfidence::Exact,
                    AcceptedEdgeConfidenceV1::Heuristic => bbox_chunker::EdgeConfidence::Heuristic,
                    AcceptedEdgeConfidenceV1::Unknown => bbox_chunker::EdgeConfidence::Unknown,
                },
            })
            .collect(),
        rationale: record.rationale.clone(),
        expires_at: record.expires_at.clone(),
        source: record.source.clone(),
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        recall_count: 0,
        last_recalled: None,
    }
}

fn hydrate_published_snapshot(publisher_root: &Path, snapshot: &mut PublishedKnowledgeSnapshot) {
    bbox_knowledge::knowledge::hydrate_repo_recall_stats(
        publisher_root,
        snapshot
            .entries
            .values_mut()
            .map(|published| &mut published.entry),
    );
}

fn insert_published_item(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    entry: KnowledgeEntry,
    published_scope: Option<PublishedScope>,
    content_hash: Option<String>,
    built_from_ref: Option<&str>,
    compatibility_lane: Option<&str>,
) {
    let entity_ref = EntityRef::Knowledge {
        id: entry.id.clone(),
    }
    .to_string();
    items.insert(
        entity_ref.clone(),
        KnowledgeViewItem {
            metadata: KnowledgeViewMetadata {
                logical_ref: entity_ref.clone(),
                published_scope,
                checkout_id: None,
                content_hash,
                overlay_snapshot_id: None,
                built_from_ref: built_from_ref.map(str::to_owned),
                compatibility_lane: compatibility_lane.map(str::to_owned),
            },
            entity_ref,
            entry,
        },
    );
}

/// How one overlay row names its project.
///
/// The bridge stamps the checkout path its records lane already carried. A
/// catalog row has no path to stamp and carries durable identity instead,
/// exactly as its published rows do.
#[derive(Debug, Clone, Copy)]
enum OverlayRowProject<'a> {
    LegacyPath(&'a str),
    Catalog(&'a str),
}

fn apply_own_overlay(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    snapshot: &OverlaySnapshot,
    project: OverlayRowProject<'_>,
    built_from_ref: Option<&str>,
) {
    for (entry_id, value) in &snapshot.values {
        items.remove(
            &EntityRef::Knowledge {
                id: entry_id.clone(),
            }
            .to_string(),
        );
        if matches!(value, OverlayValue::Upsert { .. }) {
            insert_overlay_item(items, snapshot, entry_id, value, project, built_from_ref);
        }
    }
}

fn add_overlay_upserts(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    snapshot: &OverlaySnapshot,
    project: OverlayRowProject<'_>,
    built_from_ref: Option<&str>,
) {
    for (entry_id, value) in &snapshot.values {
        if matches!(value, OverlayValue::Upsert { .. }) {
            insert_overlay_item(items, snapshot, entry_id, value, project, built_from_ref);
        }
    }
}

fn insert_overlay_item(
    items: &mut BTreeMap<String, KnowledgeViewItem>,
    snapshot: &OverlaySnapshot,
    entry_id: &str,
    value: &OverlayValue,
    project: OverlayRowProject<'_>,
    built_from_ref: Option<&str>,
) {
    let OverlayValue::Upsert {
        entry,
        content_hash,
    } = value
    else {
        return;
    };
    let entity_ref = provisional_entity_ref(
        &snapshot.key.published_scope,
        &snapshot.key.checkout_id,
        entry_id,
    );
    let mut entry = (**entry).clone();
    match project {
        OverlayRowProject::LegacyPath(path) => {
            // Legacy checkout bytes are repo-owned project knowledge. They
            // cannot assert global render authority or a catalog project id.
            entry.scope = Scope::Project;
            entry.project = Some(path.to_string());
            entry.project_id = None;
        }
        OverlayRowProject::Catalog(project_id) => {
            entry.project = None;
            entry.project_id = Some(project_id.to_string());
        }
    }
    items.insert(
        entity_ref.clone(),
        KnowledgeViewItem {
            metadata: KnowledgeViewMetadata {
                logical_ref: format!("knowledge:{entry_id}"),
                published_scope: Some(snapshot.key.published_scope.clone()),
                checkout_id: Some(snapshot.key.checkout_id.clone()),
                content_hash: Some(content_hash.clone()),
                overlay_snapshot_id: Some(snapshot.snapshot_id.clone()),
                built_from_ref: built_from_ref.map(str::to_owned),
                compatibility_lane: built_from_ref
                    .is_none()
                    .then(|| LEGACY_COMPATIBILITY_LANE.to_string()),
            },
            entity_ref,
            entry,
        },
    );
}

fn intern_overlay_stamp(
    table: &mut BuiltFromTable,
    snapshot: &OverlaySnapshot,
    diagnostics: &mut Vec<String>,
) -> Option<String> {
    let Some(stamp) = snapshot.stamp.as_ref() else {
        diagnostics.push(format!(
            "checkout {} overlay has no provable built_from stamp; rows remain in legacy_compatibility",
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

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_knowledge::knowledge::{Approval, Category, KnowledgeListParams, Priority, Status};
    use bbox_knowledge::overlay::{OverlayKey, OverlaySnapshot, OverlayStamp};
    use std::collections::HashMap;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
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

    fn entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn write_entry(root: &Path, entry: &KnowledgeEntry) {
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", entry.id)),
            serde_json::to_vec_pretty(entry).unwrap(),
        )
        .unwrap();
    }

    fn snapshot(
        scope: &PublishedScope,
        checkout_id: &str,
        values: BTreeMap<String, OverlayValue>,
    ) -> OverlaySnapshot {
        OverlaySnapshot {
            snapshot_id: format!("snapshot-{checkout_id}"),
            key: OverlayKey {
                published_scope: scope.clone(),
                checkout_id: checkout_id.into(),
            },
            stamp: Some(OverlayStamp {
                published_scope: scope.clone(),
                checkout_id: checkout_id.into(),
                published_ref: "refs/heads/main".into(),
                publisher_commit: "published-for-test".into(),
                checkout_head: format!("head-{checkout_id}"),
                merge_base: "merge-base-for-test".into(),
                working_fingerprint: format!("dirty-{checkout_id}"),
                accepted_generation: None,
            }),
            status: OverlayStatus::Valid,
            values,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn committed_view_enforces_session_authority_tombstones_and_peer_policy() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        git(&base, &["config", "user.email", "test@example.com"]);
        git(&base, &["config", "user.name", "Test"]);
        std::fs::write(base.join("README.md"), "seed\n").unwrap();
        git(&base, &["add", "README.md"]);
        git(&base, &["commit", "-q", "-m", "seed"]);
        let repo_id = crate::config::ensure_recorded_repo_id(&base).unwrap();
        let mut hostile_published = entry("shared", "PUBLISHED_CONTENT");
        hostile_published.scope = Scope::Global;
        hostile_published.project_id = Some("forged-published-project".into());
        write_entry(&base, &hostile_published);
        write_entry(&base, &entry("deleted", "PUBLISHED_DELETE_TARGET"));
        git(&base, &["add", ".bbox"]);
        git(&base, &["commit", "-q", "-m", "published knowledge"]);

        let peer_path = temp.path().join("peer");
        git(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "peer-branch",
                peer_path.to_str().unwrap(),
            ],
        );
        // Dirty publisher bytes must not redefine committed truth.
        write_entry(&base, &entry("shared", "DIRTY_PUBLISHER_CONTENT"));

        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = Arc::new(crate::server::SharedState::for_test(&state_dir));
        let project = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base)
            .unwrap();
        let server = BlackboxServer::new(state.clone());
        let scope = PublishedScope::try_new(repo_id.repo_id, ".").unwrap();

        let own_id = bbox_corpus_core::identity::ensure_checkout_id(&base).unwrap();
        let peer_id = "peer-checkout";
        write_entry(&base, &entry("shared", "OWN_CONTENT"));
        std::fs::remove_file(base.join(".bbox/knowledge/deleted.json")).unwrap();
        let mut peer_values = BTreeMap::new();
        let mut hostile_peer = entry("shared", "PEER_CONTENT");
        hostile_peer.scope = Scope::Global;
        hostile_peer.project_id = Some("forged-peer-project".into());
        peer_values.insert(
            "shared".into(),
            OverlayValue::Upsert {
                entry: Box::new(hostile_peer),
                content_hash: "peer-hash".into(),
            },
        );
        state
            .knowledge_overlays
            .write()
            .publish(snapshot(&scope, peer_id, peer_values));
        state.knowledge_overlays.write().publish(OverlaySnapshot {
            snapshot_id: String::new(),
            key: OverlayKey {
                published_scope: scope.clone(),
                checkout_id: "invalid-peer".into(),
            },
            stamp: None,
            status: OverlayStatus::Invalid,
            values: BTreeMap::new(),
            diagnostics: vec!["malformed entry".into()],
        });
        let own_checkout = ResolvedCheckoutScope {
            project_id: project.project_id,
            published_scope: scope.clone(),
            checkout_id: own_id.clone(),
            checkout_dir: base.to_string_lossy().into_owned(),
            checkout_project_dir: base.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/main".into()),
        };
        server
            .register_dark_knowledge_checkout(&own_checkout)
            .unwrap();
        server
            .session_checkout
            .set(Some(Arc::new(own_checkout.clone())))
            .unwrap();

        let published = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            published.knowledge.entry("shared").unwrap().content,
            "PUBLISHED_CONTENT"
        );
        assert_eq!(
            published.knowledge.entry("shared").unwrap().scope,
            Scope::Project
        );
        assert_eq!(
            published.knowledge.entry("shared").unwrap().project_id,
            None
        );
        assert!(published.knowledge.entry("deleted").is_some());
        std::fs::create_dir_all(base.join(".bbox/local")).unwrap();
        std::fs::write(
            base.join(".bbox/local/knowledge-stats.json"),
            r#"{"shared":{"recall_count":7,"last_recalled":"2026-07-21T00:00:00Z"}}"#,
        )
        .unwrap();
        let rehydrated = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            rehydrated.knowledge.entry("shared").unwrap().recall_count,
            7,
            "commit-keyed blob caches must rehydrate mutable recall telemetry"
        );

        let refreshed_own = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("own"))
            .expect("missing own overlay should receive one bounded refresh");
        let refreshed_own_ref = provisional_entity_ref(&scope, &own_id, "shared");
        assert_eq!(
            refreshed_own
                .knowledge
                .entry(&refreshed_own_ref)
                .unwrap()
                .content,
            "OWN_CONTENT"
        );
        assert!(refreshed_own.knowledge.entry("deleted").is_none());
        server.refresh_dark_knowledge_overlay(&own_checkout);

        // A model-supplied peer checkout path scopes the published project but
        // cannot replace the session's own checkout authority.
        let mut own = server
            .session_knowledge_view(Some(peer_path.to_str().unwrap()), Some("own"))
            .unwrap();
        let own_ref = provisional_entity_ref(&scope, &own_id, "shared");
        let peer_ref = provisional_entity_ref(&scope, peer_id, "shared");
        assert_eq!(
            own.knowledge.entry(&own_ref).unwrap().content,
            "OWN_CONTENT"
        );
        assert!(own.knowledge.entry(&peer_ref).is_none());
        assert!(own.knowledge.entry("shared").is_none());
        assert!(own.knowledge.entry("deleted").is_none());
        let listed = own
            .knowledge
            .list(&KnowledgeListParams {
                query: Some("OWN_CONTENT".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(listed.contains(&own_ref), "{listed}");

        let mut all = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("all"))
            .unwrap();
        assert!(all.knowledge.entry("shared").is_some());
        assert!(all.knowledge.entry(&own_ref).is_some());
        assert!(all.knowledge.entry(&peer_ref).is_some());
        assert_eq!(
            all.knowledge.entry(&peer_ref).unwrap().scope,
            Scope::Project
        );
        assert_eq!(all.knowledge.entry(&peer_ref).unwrap().project_id, None);
        assert_eq!(all.built_from.len(), 3);
        let published_stamp_ref = all
            .knowledge
            .view_metadata("shared")
            .and_then(|metadata| metadata.built_from_ref.clone())
            .expect("published row stamp");
        let own_stamp_ref = all
            .knowledge
            .view_metadata(&own_ref)
            .and_then(|metadata| metadata.built_from_ref.clone())
            .expect("own row stamp");
        let peer_stamp_ref = all
            .knowledge
            .view_metadata(&peer_ref)
            .and_then(|metadata| metadata.built_from_ref.clone())
            .expect("peer row stamp");
        assert_ne!(published_stamp_ref, own_stamp_ref);
        assert_ne!(published_stamp_ref, peer_stamp_ref);
        assert_ne!(own_stamp_ref, peer_stamp_ref);
        assert!(matches!(
            all.built_from.get(&peer_stamp_ref),
            Some(BuiltFromStamp::CheckoutOverlay {
                working_fingerprint,
                ..
            }) if working_fingerprint == "dirty-peer-checkout"
        ));
        let rendered = all.knowledge.list(&KnowledgeListParams::default()).unwrap();
        let rendered = all.append_built_from_for_ids(
            rendered,
            &["shared".into(), own_ref.clone(), peer_ref.clone()],
        );
        assert!(rendered.contains("built_from=built_from_"), "{rendered}");
        assert!(rendered.contains("working_fingerprint=dirty-peer-checkout"));
        let structured =
            all.structured_response(&["shared".into(), own_ref.clone(), peer_ref.clone()]);
        assert_eq!(structured["rows"].as_array().unwrap().len(), 3);
        for row in structured["rows"].as_array().unwrap() {
            let reference = row["built_from_ref"].as_str().unwrap();
            assert!(structured["built_from"].get(reference).is_some());
        }
        let (_, indexed) = all
            .enrich_json_response(
                serde_json::json!({
                    "text": "hybrid rows",
                    "results": [
                        {"entity_id": "knowledge:shared"},
                        {"entity_id": own_ref.clone()},
                        {"entity_id": peer_ref.clone()},
                        {"entity_id": "thread:unrelated"}
                    ]
                })
                .to_string(),
            )
            .unwrap();
        assert_eq!(indexed["built_from"].as_object().unwrap().len(), 3);
        assert!(indexed["results"][0]["built_from_ref"].is_string());
        assert!(indexed["results"][1]["built_from_ref"].is_string());
        assert!(indexed["results"][2]["built_from_ref"].is_string());
        assert!(indexed["results"][3].get("built_from_ref").is_none());
        assert!(
            indexed["text"]
                .as_str()
                .unwrap()
                .contains("working_fingerprint=")
        );
        let pinned_publisher_commit = match all.built_from.get(&published_stamp_ref).unwrap() {
            BuiltFromStamp::Published {
                published_ref,
                publisher_commit,
                ..
            } => {
                assert_eq!(published_ref, "refs/heads/main");
                publisher_commit.clone()
            }
            other => panic!("expected published stamp, got {other:?}"),
        };
        let diagnostics = all.diagnostics_text().unwrap();
        assert!(diagnostics.contains("invalid-peer"), "{diagnostics}");
        assert!(
            diagnostics.contains("tombstones knowledge:deleted"),
            "{diagnostics}"
        );

        std::fs::write(peer_path.join(".bbox/knowledge/shared.json"), "not-json").unwrap();
        let invalid_server = BlackboxServer::new(state);
        let invalid_checkout = ResolvedCheckoutScope {
            project_id: "test-project".into(),
            published_scope: scope,
            checkout_id: "invalid-peer".into(),
            checkout_dir: peer_path.to_string_lossy().into_owned(),
            checkout_project_dir: peer_path.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/peer-branch".into()),
        };
        invalid_server
            .session_checkout
            .set(Some(Arc::new(invalid_checkout.clone())))
            .unwrap();
        invalid_server
            .register_dark_knowledge_checkout(&invalid_checkout)
            .unwrap();
        invalid_server.refresh_dark_knowledge_overlay(&invalid_checkout);
        let error = invalid_server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("own"))
            .err()
            .expect("invalid own overlay must fail closed");
        assert!(
            error
                .to_string()
                .contains("own checkout overlay is invalid")
        );

        write_entry(&base, &entry("shared", "NEW_PUBLISHED_CONTENT"));
        git(&base, &["add", ".bbox/knowledge"]);
        git(
            &base,
            &["commit", "-q", "-m", "advance published knowledge"],
        );
        server.refresh_dark_knowledge_overlay(&own_checkout);
        server.state.index_writer.flush_blocking().unwrap();
        let refreshed = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("published"))
            .unwrap();
        assert_eq!(
            refreshed.knowledge.entry("shared").unwrap().content,
            "NEW_PUBLISHED_CONTENT"
        );
        let refreshed_stamp_ref = refreshed
            .knowledge
            .view_metadata("shared")
            .and_then(|metadata| metadata.built_from_ref.as_deref())
            .unwrap();
        let refreshed_publisher_commit = match refreshed.built_from.get(refreshed_stamp_ref) {
            Some(BuiltFromStamp::Published {
                publisher_commit, ..
            }) => publisher_commit,
            other => panic!("expected refreshed published stamp, got {other:?}"),
        };
        assert_ne!(refreshed_publisher_commit, &pinned_publisher_commit);
        assert!(all.built_from.iter().any(|(_, stamp)| matches!(
            stamp,
            BuiltFromStamp::Published {
                publisher_commit,
                ..
            } if publisher_commit == &pinned_publisher_commit
        )));
        let all = server
            .session_knowledge_view(Some(base.to_str().unwrap()), Some("all"))
            .unwrap();
        assert!(
            all.knowledge.entry(&own_ref).is_none(),
            "the matching checkout variant must promote away"
        );
        assert_eq!(
            all.knowledge.entry(&peer_ref).unwrap().content,
            "PEER_CONTENT",
            "publisher advancement must preserve another checkout's variant"
        );

        let published_hits = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("NEW PUBLISHED CONTENT", 10, Some("knowledge"))
            .unwrap();
        assert!(
            published_hits
                .iter()
                .any(|hit| hit.entity_id == crate::index::knowledge_entity_id("shared")),
            "{published_hits:?}"
        );
        let peer_hits = server
            .state
            .idx
            .read()
            .hybrid_bm25_hits("PEER CONTENT", 10, Some("knowledge"))
            .unwrap();
        assert!(
            !peer_hits.iter().any(|hit| hit.entity_id == peer_ref),
            "static corpus search must not expose checkout-only knowledge: {peer_hits:?}"
        );
        assert!(all.knowledge.entry(&peer_ref).is_some(), "{peer_hits:?}");
    }

    #[test]
    fn pre_cut_legacy_view_is_visible_and_bounded_by_registered_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        for root in [&first, &second] {
            std::fs::create_dir_all(root).unwrap();
            git(root, &["init", "-q", "-b", "main"]);
            git(root, &["config", "user.email", "test@example.com"]);
            git(root, &["config", "user.name", "Test"]);
            std::fs::write(root.join("README.md"), "seed\n").unwrap();
            git(root, &["add", "README.md"]);
            git(root, &["commit", "-q", "-m", "seed"]);
        }

        let state_dir = temp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = Arc::new(crate::server::SharedState::for_test(&state_dir));
        let first_record = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&first)
            .unwrap();
        let second_record = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&second)
            .unwrap();
        assert!(
            bbox_indexing::publisher::project_published_scope(
                &first_record,
                crate::config::read_repo_id_inputs,
            )
            .is_none(),
            "the fixture must not have recorded repo identity"
        );
        assert!(
            bbox_indexing::publisher::project_published_scope(
                &second_record,
                crate::config::read_repo_id_inputs,
            )
            .is_none(),
            "the fixture must not have recorded repo identity"
        );

        let mut first_entry = entry("first-legacy", "FIRST_LEGACY_CONTENT");
        first_entry.project = Some(first_record.canonical_path.clone());
        state.kb.write().upsert_generated(first_entry).unwrap();
        let mut second_entry = entry("second-legacy", "SECOND_LEGACY_CONTENT");
        second_entry.project = Some(second_record.canonical_path.clone());
        state.kb.write().upsert_generated(second_entry).unwrap();

        let server = BlackboxServer::new(state);
        let compatibility_diagnostic =
            "legacy_compatibility knowledge rows have no provable built_from stamp";
        let aggregate = server.session_knowledge_view(None, None).unwrap();
        assert!(aggregate.knowledge.entry("first-legacy").is_some());
        assert!(aggregate.knowledge.entry("second-legacy").is_some());
        assert_eq!(
            aggregate.diagnostics,
            vec![compatibility_diagnostic.to_owned()]
        );

        let explicit = server
            .session_knowledge_view(Some(&first_record.canonical_path), None)
            .unwrap();
        assert!(explicit.knowledge.entry("first-legacy").is_some());
        assert!(
            explicit.knowledge.entry("second-legacy").is_none(),
            "an explicit read must not expose another registered legacy scope"
        );
        assert_eq!(
            explicit.diagnostics,
            vec![compatibility_diagnostic.to_owned()]
        );
    }
}

/// Catalog published knowledge views (Phase 5 plan section 8, P5-B).
#[cfg(test)]
mod catalog_view_tests {
    use crate::server::state::catalog_fixture::{
        COMMIT_ONE, COMMIT_TWO, CatalogFixture, gap_note, knowledge_entry,
    };

    use super::*;

    fn published_stamp(view: &SessionKnowledgeView, entry_id: &str) -> BuiltFromStamp {
        let reference = view
            .knowledge
            .view_metadata(entry_id)
            .and_then(|metadata| metadata.built_from_ref.clone())
            .expect("catalog published rows carry a built_from stamp");
        view.built_from
            .get(&reference)
            .cloned()
            .expect("the stamp reference resolves in the view table")
    }

    fn row(view: &SessionKnowledgeView, entry_id: &str) -> KnowledgeViewItem {
        view.items
            .iter()
            .find(|item| item.entry.id == entry_id)
            .cloned()
            .expect("row is present")
    }

    /// A crash between the pointer swap and the index commit must not
    /// leave search on the old generation forever (R2-2).
    ///
    /// The daemon converges the index asynchronously and persists no
    /// record of having done so, so the only thing that can repair a lost
    /// convergence is a reprojection from the pointer at boot.
    #[test]
    fn startup_convergence_repairs_an_index_left_behind_by_a_crash() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_crash", &scope);
        fixture.install_publication(
            "p_crash",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "generationone")],
            &[],
        );

        // Boot one: converge the index at G1.
        let first = fixture.server();
        first.state.install_code_read_view_commit_hook();
        let project_id = ProjectId::parse("p_crash").unwrap();
        first.converge_published_knowledge_index(&project_id);
        first.state.index_writer.flush_blocking().unwrap();
        first.state.idx.write().reader_reload_for_test();
        assert!(index_search(&first, "generationone").contains("knowledge-a"));

        // The pointer advances to G2 and the process dies before the scope
        // replacement commits: no convergence runs for G2 at all.
        fixture.install_publication(
            "p_crash",
            &scope,
            COMMIT_TWO,
            &[knowledge_entry("knowledge-a", "generationtwo")],
            &[],
        );
        drop(first);

        // Boot two, with the project remote-only: no attachment exists
        // anywhere, so the repair may only read durable accepted content.
        let second = fixture.server();
        second.state.install_code_read_view_commit_hook();
        assert!(
            second
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
        // The crash state is real: before the repair, search still answers
        // with the superseded generation while accepted reads answer with
        // the new one.
        assert!(index_search(&second, "generationone").contains("knowledge-a"));
        assert!(!index_search(&second, "generationtwo").contains("knowledge-a"));
        assert_eq!(
            second
                .session_knowledge_view(None, None)
                .unwrap()
                .items
                .first()
                .unwrap()
                .entry
                .content,
            "generationtwo"
        );

        let report = second.converge_published_knowledge_at_startup();
        assert_eq!(report.visited, 1);
        assert_eq!(report.converged, 1);
        assert_eq!(report.skipped, 0);
        second.state.index_writer.flush_blocking().unwrap();
        second.state.idx.write().reader_reload_for_test();

        assert!(
            index_search(&second, "generationtwo").contains("knowledge-a"),
            "search must serve the generation the pointer names"
        );
        assert!(
            !index_search(&second, "generationone").contains("knowledge-a"),
            "the superseded generation must not survive in the index"
        );
    }

    /// A project whose publication cannot be verified is skipped, never
    /// cleared: a prior-generation fallback may still be serving it.
    #[test]
    fn startup_convergence_skips_projects_without_verified_content() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project("p_nopublication", &CatalogFixture::scope("."));
        let server = fixture.server();

        let report = server.converge_published_knowledge_at_startup();
        assert_eq!(report.visited, 1);
        assert_eq!(report.converged, 0);
        assert_eq!(report.skipped, 1);
    }

    fn index_search(server: &BlackboxServer, query: &str) -> String {
        let view = server.state.code_read_view.read().clone();
        server
            .state
            .idx
            .read()
            .search_with_active_selectors_and_searcher(
                &crate::index::SearchParams {
                    query: query.into(),
                    mode: None,
                    account: None,
                    project: None,
                    role: None,
                    include_subagents: None,
                    limit: Some(5),
                    source: None,
                    author: None,
                    channel: None,
                    exclude_self: None,
                },
                &view.active_selectors,
                &view.searcher,
            )
            .unwrap()
    }

    #[test]
    fn a_remote_only_catalog_project_serves_accepted_knowledge_with_no_lease() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_remote", &scope);
        let installed = fixture.install_publication(
            "p_remote",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "accepted content")],
            &[gap_note("gap-1234abcd", "accepted gap")],
        );
        let server = fixture.server();

        let view = server.session_knowledge_view(None, None).unwrap();
        let item = row(&view, "knowledge-a");
        assert_eq!(item.entry.content, "accepted content");
        // Identity travels as a project id; the path field stays empty
        // because a catalog read has no checkout.
        assert_eq!(item.entry.project_id.as_deref(), Some("p_remote"));
        assert_eq!(item.entry.project, None);
        // Recall telemetry is repo-local and advisory: the source entry
        // carried counts, accepted normalization dropped them, and the
        // catalog read must not reopen a checkout to restore them.
        assert_eq!(item.entry.recall_count, 0);
        assert_eq!(item.entry.last_recalled, None);
        assert_eq!(item.metadata.published_scope.as_ref(), Some(&scope));
        assert!(item.metadata.content_hash.is_some());

        assert_eq!(
            published_stamp(&view, "knowledge-a"),
            BuiltFromStamp::Published {
                published_scope: scope.clone(),
                published_ref: "refs/heads/main".into(),
                publisher_commit: COMMIT_ONE.into(),
            }
        );
        assert!(!installed.generation_id.is_empty());

        // Published reads never enter the checkout plane. The broker is a
        // deny probe, so any acquisition would also have failed the read.
        let health = server.state.checkout_access.health();
        assert!(
            health
                .operations
                .iter()
                .all(|operation| operation.granted == 0 && operation.denied == 0)
        );
        // The version-1 lane is not merely unused, it is untouched: no
        // publisher authorization was resolved and no scope-keyed published
        // snapshot was loaded. Both are the entry points to publisher
        // election, the publisher root, Git, and recall hydration, so an
        // empty pair is the negative proof for all four.
        assert!(server.state.publisher_authorization_cache.read().is_empty());
        assert!(server.state.knowledge_published_cache.read().is_empty());
        assert!(server.state.gap_published_cache.read().is_empty());
    }

    #[test]
    fn a_rebind_changes_binding_identity_without_evicting_projected_content() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_rebind", &scope);
        fixture.install_publication(
            "p_rebind",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "accepted content")],
            &[],
        );
        let server = fixture.server();
        let project_id = ProjectId::parse("p_rebind").unwrap();

        server.session_knowledge_view(None, None).unwrap();
        let before = server
            .state
            .catalog_knowledge_published_cache
            .read()
            .get(&project_id)
            .expect("the first read installs a projected snapshot")
            .content_stamp
            .clone();

        // Attachment-only rebind: the pointer bytes and their digest
        // change, the accepted content does not.
        fixture.rebind("p_rebind", "att_22222222222222222222222222222222");
        server
            .state
            .accepted_publications
            .as_ref()
            .unwrap()
            .invalidate_binding(&project_id);

        let after = server.session_knowledge_view(None, None).unwrap();
        assert_eq!(row(&after, "knowledge-a").entry.content, "accepted content");
        assert_eq!(
            server
                .state
                .catalog_knowledge_published_cache
                .read()
                .get(&project_id)
                .unwrap()
                .content_stamp,
            before,
            "a binding change must not change content identity"
        );
        assert_eq!(
            published_stamp(&after, "knowledge-a"),
            BuiltFromStamp::Published {
                published_scope: scope,
                published_ref: "refs/heads/main".into(),
                publisher_commit: COMMIT_ONE.into(),
            }
        );
    }

    #[test]
    fn a_restart_serves_the_same_accepted_generation() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_restart", &scope);
        fixture.install_publication(
            "p_restart",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "generation one")],
            &[],
        );

        let first = fixture.server().session_knowledge_view(None, None).unwrap();
        // A second server over the same durable bytes is a restart: new
        // runtime, empty caches, no attachment anywhere in the story.
        let second = fixture.server().session_knowledge_view(None, None).unwrap();
        assert_eq!(
            row(&first, "knowledge-a").entry.content,
            row(&second, "knowledge-a").entry.content
        );
        assert_eq!(
            row(&first, "knowledge-a").metadata.content_hash,
            row(&second, "knowledge-a").metadata.content_hash
        );
        assert_eq!(
            published_stamp(&first, "knowledge-a"),
            published_stamp(&second, "knowledge-a")
        );
    }

    #[test]
    fn a_project_without_a_pointer_reports_publication_unavailable() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project("p_nopublication", &CatalogFixture::scope("."));
        let server = fixture.server();

        let view = server.session_knowledge_view(None, None).unwrap();
        assert!(view.items.is_empty());
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
    fn one_corrupt_project_does_not_hide_a_healthy_peer() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        // One published scope is one project: the catalog refuses a
        // duplicate, so peers live at distinct `.bbox` roots.
        let broken_scope = CatalogFixture::scope("sub/broken");
        fixture.add_published_project("p_healthy", &scope);
        fixture.add_published_project("p_broken", &broken_scope);
        fixture.install_publication(
            "p_healthy",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "healthy")],
            &[],
        );
        let broken = fixture.install_publication(
            "p_broken",
            &broken_scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-b", "broken")],
            &[],
        );
        fixture.corrupt_generation("p_broken", &broken.generation_id);
        let server = fixture.server();

        let view = server.session_knowledge_view(None, None).unwrap();
        assert_eq!(row(&view, "knowledge-a").entry.content, "healthy");
        assert!(view.items.iter().all(|item| item.entry.id != "knowledge-b"));
        assert!(
            view.diagnostics.iter().any(|diagnostic| {
                diagnostic.contains("p_broken") && diagnostic.contains("unavailable")
            }),
            "{:?}",
            view.diagnostics
        );
    }

    #[test]
    fn a_prior_fallback_serves_prior_rows_and_reports_repair() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_prior", &scope);
        let first = fixture.install_publication(
            "p_prior",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "first generation")],
            &[],
        );
        let second = fixture.install_publication(
            "p_prior",
            &scope,
            COMMIT_TWO,
            &[knowledge_entry("knowledge-a", "second generation")],
            &[],
        );
        fixture.corrupt_generation("p_prior", &second.generation_id);
        let server = fixture.server();

        let view = server.session_knowledge_view(None, None).unwrap();
        assert_eq!(
            row(&view, "knowledge-a").entry.content,
            "first generation",
            "a damaged current arm serves the prior generation"
        );
        // The response provenance names the generation that actually
        // served, not the pointer's damaged head.
        assert_eq!(
            published_stamp(&view, "knowledge-a"),
            BuiltFromStamp::Published {
                published_scope: scope,
                published_ref: "refs/heads/main".into(),
                publisher_commit: COMMIT_ONE.into(),
            }
        );
        assert_ne!(first.generation_id, second.generation_id);
        assert!(
            view.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("served from the prior generation")),
            "{:?}",
            view.diagnostics
        );
    }

    #[test]
    fn scope_migration_keeps_the_old_accepted_scope_until_advance() {
        let fixture = CatalogFixture::new();
        let accepted_scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_scope", &accepted_scope);
        fixture.install_publication(
            "p_scope",
            &accepted_scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "old scope content")],
            &[],
        );
        fixture.migrate_project_scope("p_scope", &CatalogFixture::scope("sub/project"));
        let server = fixture.server();

        let view = server.session_knowledge_view(None, None).unwrap();
        // No accepted snapshot is ever relabeled: the response keeps the
        // scope its content was published at.
        assert_eq!(
            published_stamp(&view, "knowledge-a"),
            BuiltFromStamp::Published {
                published_scope: accepted_scope.clone(),
                published_ref: "refs/heads/main".into(),
                publisher_commit: COMMIT_ONE.into(),
            }
        );
        assert_eq!(
            row(&view, "knowledge-a").metadata.published_scope.as_ref(),
            Some(&accepted_scope)
        );
        assert!(
            view.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("new-scope advance")),
            "{:?}",
            view.diagnostics
        );
    }

    #[test]
    fn the_content_cache_survives_repeat_reads_and_advance_replaces_it() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_cache", &scope);
        fixture.install_publication(
            "p_cache",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "generation one")],
            &[],
        );
        let server = fixture.server();
        let project_id = ProjectId::parse("p_cache").unwrap();

        server.session_knowledge_view(None, None).unwrap();
        let first_stamp = server
            .state
            .catalog_knowledge_published_cache
            .read()
            .get(&project_id)
            .expect("the first read installs a projected snapshot")
            .content_stamp
            .clone();
        server.session_knowledge_view(None, None).unwrap();
        assert_eq!(
            server
                .state
                .catalog_knowledge_published_cache
                .read()
                .get(&project_id)
                .unwrap()
                .content_stamp,
            first_stamp,
            "a repeat read reuses the projection instead of rebuilding it"
        );

        fixture.install_publication(
            "p_cache",
            &scope,
            COMMIT_TWO,
            &[knowledge_entry("knowledge-a", "generation two")],
            &[],
        );
        // Advance is what invalidates content. Without it the runtime keeps
        // serving the generation it verified, which is the documented
        // caching contract, not a staleness bug.
        assert_eq!(
            row(
                &server.session_knowledge_view(None, None).unwrap(),
                "knowledge-a"
            )
            .entry
            .content,
            "generation one"
        );
        server.invalidate_catalog_published_content(&project_id);
        let after = server.session_knowledge_view(None, None).unwrap();
        assert_eq!(row(&after, "knowledge-a").entry.content, "generation two");
        assert_ne!(
            server
                .state
                .catalog_knowledge_published_cache
                .read()
                .get(&project_id)
                .unwrap()
                .content_stamp,
            first_stamp
        );
        assert_eq!(
            published_stamp(&after, "knowledge-a"),
            BuiltFromStamp::Published {
                published_scope: scope,
                published_ref: "refs/heads/main".into(),
                publisher_commit: COMMIT_TWO.into(),
            }
        );
    }

    #[test]
    fn an_explicit_project_selector_narrows_to_one_catalog_project() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        let second_scope = CatalogFixture::scope("sub/second");
        fixture.add_published_project("p_first", &scope);
        fixture.add_published_project("p_second", &second_scope);
        fixture.install_publication(
            "p_first",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "first project")],
            &[],
        );
        fixture.install_publication(
            "p_second",
            &second_scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-b", "second project")],
            &[],
        );
        let server = fixture.server();

        let view = server
            .session_knowledge_view(Some("p_first"), None)
            .unwrap();
        assert_eq!(row(&view, "knowledge-a").entry.content, "first project");
        assert!(view.items.iter().all(|item| item.entry.id != "knowledge-b"));
    }
}

/// Catalog overlay baseline path (Phase 5 plan sections 8 P5-D and 13.4).
///
/// Every fixture here uses a real repository: the catalog overlay proves
/// commit containment and a merge base inside one checkout and nowhere
/// else, so synthetic ancestry would prove nothing.
#[cfg(test)]
mod catalog_overlay_tests {
    use std::path::PathBuf;
    use std::process::Command;

    use bbox_indexing::accepted_publication_runtime::AcceptedPublicationSelection;

    use crate::server::state::catalog_fixture::{CatalogFixture, knowledge_entry};

    use super::*;

    const PROJECT: &str = "p_overlay";
    const BASE_ATTACHMENT: &str = "att_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01";
    const BASE_CHECKOUT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01";
    const PEER_ATTACHMENT: &str = "att_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa02";
    const PEER_CHECKOUT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa02";

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

    /// Write one entry with the exact bytes a WRITER commits.
    ///
    /// An accepted generation records the SHA-256 of the committed blob,
    /// so in production the repository content and the accepted source
    /// bytes ARE the same bytes. Committing one serialization and
    /// publishing another makes every published digest miss, which
    /// silently disables the byte-equality suppression rule instead of
    /// failing.
    ///
    /// The reference has to be the writer, not the fixture. This helper
    /// and `install_publication` previously shared one PRIVATE encoding:
    /// they agreed with each other and both disagreed with production, so
    /// the digests matched and the rule looked exercised while nothing
    /// production writes was ever compared.
    fn write_entry(root: &Path, entry: &KnowledgeEntry) {
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", entry.id)),
            bbox_knowledge::knowledge::committed_knowledge_entry_bytes(entry).unwrap(),
        )
        .unwrap();
    }

    struct OverlayFixture {
        catalog: CatalogFixture,
        _temp: tempfile::TempDir,
        root: PathBuf,
        base: PathBuf,
        accepted_commit: String,
        scope: PublishedScope,
    }

    impl OverlayFixture {
        /// One published repository committed at the accepted commit and
        /// attached as the project's base checkout.
        fn new(entries: &[KnowledgeEntry]) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let base = root.join("base");
            std::fs::create_dir_all(&base).unwrap();
            git(&base, &["init", "-q", "-b", "main"]);
            git(&base, &["config", "user.email", "t@example.com"]);
            git(&base, &["config", "user.name", "Test"]);
            for entry in entries {
                write_entry(&base, entry);
            }
            git(&base, &["add", ".bbox/knowledge"]);
            git(&base, &["commit", "-q", "-m", "accepted"]);
            let accepted_commit = bbox_corpus_core::git::current_head(&base).unwrap();

            let catalog = CatalogFixture::new();
            let scope = CatalogFixture::scope(".");
            catalog.add_published_project(PROJECT, &scope);
            catalog.install_publication(PROJECT, &scope, &accepted_commit, entries, &[]);
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

        /// A worktree branched off the accepted commit: the ordinary shape
        /// of a peer that can prove the baseline.
        fn worktree(&self, name: &str, attachment_id: &str, checkout_id: &str) -> PathBuf {
            let path = self.root.join(name);
            git(
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

        /// A repository with its own unrelated history. It cannot contain
        /// the accepted commit, and there is no publisher root to borrow
        /// it from (D-007).
        fn unrelated(&self, name: &str, attachment_id: &str, checkout_id: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            git(&path, &["init", "-q", "-b", "main"]);
            git(&path, &["config", "user.email", "t@example.com"]);
            git(&path, &["config", "user.name", "Test"]);
            write_entry(&path, &knowledge_entry("keep", "unrelated"));
            git(&path, &["add", ".bbox/knowledge"]);
            git(&path, &["commit", "-q", "-m", "unrelated"]);
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

    fn provisional_row(view: &SessionKnowledgeView, entry_id: &str) -> KnowledgeViewItem {
        view.items
            .iter()
            .find(|item| {
                item.entry.id == entry_id && item.entity_ref.starts_with("provisional_knowledge:")
            })
            .cloned()
            .unwrap_or_else(|| panic!("provisional row {entry_id} is present: {:?}", view.items))
    }

    #[test]
    fn an_attached_checkout_positions_its_working_tree_against_accepted_content() {
        let fixture = OverlayFixture::new(&[
            knowledge_entry("keep", "accepted"),
            knowledge_entry("remove", "accepted"),
        ]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_entry(
            &worktree,
            &knowledge_entry("keep", "changed in the checkout"),
        );
        write_entry(&worktree, &knowledge_entry("new", "untracked"));
        std::fs::remove_file(worktree.join(".bbox/knowledge/remove.json")).unwrap();

        let server = fixture.catalog.server_with_checkout_authority();
        server.set_session_checkout_for_test(
            PROJECT.into(),
            fixture.scope.clone(),
            PEER_CHECKOUT.into(),
            worktree.clone(),
        );

        let view = server.session_knowledge_view(None, Some("own")).unwrap();
        assert_eq!(
            provisional_row(&view, "keep").entry.content,
            "changed in the checkout"
        );
        assert_eq!(provisional_row(&view, "new").entry.content, "untracked");
        assert!(
            view.items.iter().all(|item| item.entry.id != "remove"),
            "a tombstoned entry leaves the own view: {:?}",
            view.items
        );

        // A catalog overlay row carries durable identity, never a host
        // path: the checkout directory is not authority.
        let keep = provisional_row(&view, "keep");
        assert_eq!(keep.entry.project, None);
        assert_eq!(keep.entry.project_id.as_deref(), Some(PROJECT));

        let stamp_ref = keep
            .metadata
            .built_from_ref
            .clone()
            .expect("an overlay row carries a provable stamp");
        let stamp = view.built_from.get(&stamp_ref).cloned().unwrap();
        let BuiltFromStamp::CheckoutOverlay {
            checkout_id,
            publisher_commit,
            merge_base,
            ..
        } = stamp
        else {
            panic!("an overlay row stamps CheckoutOverlay: {stamp:?}");
        };
        assert_eq!(checkout_id, PEER_CHECKOUT);
        // Accepted content supplies commit P; the baseline is proved in
        // this checkout alone, and a branch off P has P as its merge base.
        assert_eq!(publisher_commit, fixture.accepted_commit);
        assert_eq!(merge_base, fixture.accepted_commit);
        assert!(view.degraded_overlays.is_empty());
    }

    /// The digest map the diff consults is keyed by BASENAME, while the
    /// accepted manifest keys are repository-relative. Feeding a manifest
    /// key straight through makes every published file look absent, which
    /// suppresses nothing and tombstones nothing: wrong answers rather
    /// than errors, so only a behavioral test catches it.
    ///
    /// A published scope below the repository root gives the manifest key
    /// a real directory prefix, and the two questions the diff asks about
    /// published content are exercised separately: does this file exist
    /// there, and do the working bytes already equal it.
    #[test]
    fn a_nested_published_scope_still_suppresses_and_tombstones_by_basename() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let base = root.join("base");
        let nested = |dir: &Path| dir.join("sub");
        std::fs::create_dir_all(nested(&base)).unwrap();
        git(&base, &["init", "-q", "-b", "main"]);
        git(&base, &["config", "user.email", "t@example.com"]);
        git(&base, &["config", "user.name", "Test"]);

        // One commit BEFORE accepted content, so the checkout's baseline
        // and accepted content genuinely disagree. A worktree branched off
        // the accepted commit would make baseline == published and the
        // suppression question would never be asked.
        write_entry(&nested(&base), &knowledge_entry("reapplied", "older"));
        write_entry(&nested(&base), &knowledge_entry("remove", "published"));
        git(&base, &["add", "sub/.bbox/knowledge"]);
        git(&base, &["commit", "-q", "-m", "before accepted"]);
        let branch_point = bbox_corpus_core::git::current_head(&base).unwrap();

        let accepted = [
            knowledge_entry("reapplied", "accepted"),
            knowledge_entry("remove", "published"),
        ];
        write_entry(&nested(&base), &accepted[0]);
        git(&base, &["add", "sub/.bbox/knowledge"]);
        git(&base, &["commit", "-q", "-m", "accepted"]);
        let accepted_commit = bbox_corpus_core::git::current_head(&base).unwrap();

        let catalog = CatalogFixture::new();
        let scope = CatalogFixture::scope("sub");
        catalog.add_published_project(PROJECT, &scope);
        catalog.install_publication(PROJECT, &scope, &accepted_commit, &accepted, &[]);

        let worktree = root.join("peer");
        git(
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

        // The checkout re-applies exactly what accepted content holds. Its
        // baseline still says "older", so the row survives the
        // baseline-equality shortcut and only the published digest can
        // suppress it.
        write_entry(
            &nested(&worktree),
            &knowledge_entry("reapplied", "accepted"),
        );
        std::fs::remove_file(nested(&worktree).join(".bbox/knowledge/remove.json")).unwrap();

        let server = catalog.server_with_checkout_authority();
        let verified = server
            .state
            .accepted_publications
            .as_ref()
            .unwrap()
            .load_verified(&ProjectId::parse(PROJECT).unwrap())
            .unwrap();
        // The manifest really does carry the prefix this test exists for.
        assert!(
            verified
                .knowledge_manifest()
                .keys()
                .all(|filename| filename.as_str().starts_with("sub/.bbox/knowledge/")),
            "the fixture must produce directory-prefixed manifest keys"
        );

        let snapshot = server
            .refresh_catalog_knowledge_overlay(
                &verified,
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
            )
            .unwrap();
        assert_eq!(snapshot.stamp.as_ref().unwrap().merge_base, branch_point);
        assert!(
            !snapshot.values.contains_key("reapplied"),
            "working bytes equal to published content are already integrated: {:?}",
            snapshot.values
        );
        assert!(
            matches!(snapshot.values.get("remove"), Some(OverlayValue::Tombstone)),
            "a deletion of a published file tombstones it: {:?}",
            snapshot.values
        );
    }

    #[test]
    fn a_detached_attachment_refuses_own_while_published_content_keeps_serving() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        fixture.catalog.detach(BASE_ATTACHMENT);
        let server = fixture.catalog.server_with_checkout_authority();
        server.set_session_checkout_for_test(
            PROJECT.into(),
            fixture.scope.clone(),
            BASE_CHECKOUT.into(),
            fixture.base.clone(),
        );

        let error = server
            .session_knowledge_view(None, Some("own"))
            .err()
            .expect("own has no honest answer without its own checkout");
        let text = format!("{error:#}");
        assert!(
            text.contains(ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE),
            "{text}"
        );
        assert!(text.contains("attachment_inactive"), "{text}");

        // Detach does not touch accepted content, which is the whole point
        // of durable publication.
        let published = server
            .session_knowledge_view(None, Some("published"))
            .unwrap();
        assert_eq!(
            published
                .items
                .iter()
                .find(|item| item.entry.id == "keep")
                .unwrap()
                .entry
                .content,
            "accepted"
        );
        assert!(published.degraded_overlays.is_empty());
    }

    #[test]
    fn a_checkout_that_cannot_prove_the_baseline_refuses_own() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        let unrelated = fixture.unrelated("unrelated", PEER_ATTACHMENT, PEER_CHECKOUT);

        let server = fixture.catalog.server_with_checkout_authority();
        server.set_session_checkout_for_test(
            PROJECT.into(),
            fixture.scope.clone(),
            PEER_CHECKOUT.into(),
            unrelated,
        );

        let error = server
            .session_knowledge_view(None, Some("own"))
            .err()
            .expect("a checkout without commit P cannot position itself");
        let text = format!("{error:#}");
        assert!(
            text.contains(ERROR_PROVISIONAL_OVERLAY_UNAVAILABLE),
            "{text}"
        );
        assert!(text.contains(ERROR_OVERLAY_BASELINE_UNAVAILABLE), "{text}");
    }

    #[test]
    fn all_omits_only_the_failed_peer_and_reports_its_reason() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        let peer = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_entry(&peer, &knowledge_entry("keep", "peer variant"));
        const BROKEN_ATTACHMENT: &str = "att_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa03";
        const BROKEN_CHECKOUT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa03";
        fixture.unrelated("broken", BROKEN_ATTACHMENT, BROKEN_CHECKOUT);

        let server = fixture.catalog.server_with_checkout_authority();
        let view = server.session_knowledge_view(None, Some("all")).unwrap();

        // Accepted content and the healthy peer both keep serving.
        assert!(
            view.items
                .iter()
                .any(|item| item.entry.id == "keep" && item.entity_ref.starts_with("knowledge:"))
        );
        assert_eq!(provisional_row(&view, "keep").entry.content, "peer variant");

        assert_eq!(
            view.degraded_overlays.len(),
            1,
            "{:?}",
            view.degraded_overlays
        );
        let degraded = &view.degraded_overlays[0];
        assert_eq!(degraded.checkout_id, BROKEN_CHECKOUT);
        assert_eq!(degraded.attachment_id.as_deref(), Some(BROKEN_ATTACHMENT));
        assert_eq!(degraded.code, ERROR_OVERLAY_BASELINE_UNAVAILABLE);
        assert!(
            view.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(ERROR_OVERLAY_BASELINE_UNAVAILABLE)),
            "{:?}",
            view.diagnostics
        );

        // The structured report carries the same bounded rows and no path.
        let structured = view.structured_response(&["keep".into()]);
        let overlays = structured["degraded"]["overlays"].as_array().unwrap();
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0]["code"], ERROR_OVERLAY_BASELINE_UNAVAILABLE);
        assert!(
            !structured
                .to_string()
                .contains(fixture.root.to_str().unwrap()),
            "a degradation must not carry an absolute path"
        );
    }

    #[test]
    fn published_ignores_overlay_failure_entirely() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        fixture.unrelated("broken", PEER_ATTACHMENT, PEER_CHECKOUT);

        let server = fixture.catalog.server_with_checkout_authority();
        let view = server
            .session_knowledge_view(None, Some("published"))
            .unwrap();

        assert_eq!(
            view.items
                .iter()
                .find(|item| item.entry.id == "keep")
                .unwrap()
                .entry
                .content,
            "accepted"
        );
        assert!(view.degraded_overlays.is_empty());
        // Published never opens a checkout at all, so a broken peer cannot
        // even be observed from this mode.
        let health = server.state.checkout_access.health();
        assert!(
            health
                .operations
                .iter()
                .all(|operation| operation.granted == 0 && operation.denied == 0)
        );
    }

    #[test]
    fn a_prior_generation_positions_the_checkout_against_prior_accepted_content() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "generation one")]);
        // Advance to a second real commit, then destroy that generation:
        // the pointer falls back to its prior arm.
        write_entry(&fixture.base, &knowledge_entry("keep", "generation two"));
        git(&fixture.base, &["add", ".bbox/knowledge"]);
        git(&fixture.base, &["commit", "-q", "-m", "advance"]);
        let second_commit = bbox_corpus_core::git::current_head(&fixture.base).unwrap();
        let installed = fixture.catalog.install_publication(
            PROJECT,
            &fixture.scope,
            &second_commit,
            &[knowledge_entry("keep", "generation two")],
            &[],
        );
        fixture
            .catalog
            .corrupt_generation(PROJECT, &installed.generation_id);

        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_entry(&worktree, &knowledge_entry("keep", "checkout variant"));

        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);
        assert_eq!(
            verified.binding_stamp().selection(),
            AcceptedPublicationSelection::Prior
        );

        let snapshot = server
            .refresh_catalog_knowledge_overlay(
                &verified,
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
            )
            .unwrap();
        let stamp = snapshot.stamp.expect("a valid snapshot carries its stamp");
        // The overlay positions the checkout against the content the
        // pointer is actually serving, which is the prior generation.
        assert_eq!(stamp.publisher_commit, fixture.accepted_commit);
        assert_eq!(
            stamp.accepted_generation.as_deref(),
            Some(verified.content_stamp().generation_id())
        );
    }

    /// The capture is a two-read agreement: head, merge base, and working
    /// fingerprint all ride the snapshot id, so one comparison covers
    /// every kind of movement.
    #[test]
    fn a_head_that_moves_once_during_capture_settles_on_the_later_read() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);
        let published = accepted_knowledge_digests(&verified);
        let content_stamp = verified.content_stamp();
        let lease = server
            .acquire_catalog_overlay_lease(
                &fixture.project_id(),
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
            )
            .unwrap();

        let mut moves = 0;
        let snapshot = stable_catalog_knowledge_overlay(
            CatalogOverlayPublished {
                published_scope: &fixture.scope,
                checkout_id: PEER_CHECKOUT,
                full_ref: content_stamp.full_ref(),
                accepted_commit: content_stamp.accepted_commit(),
                accepted_generation: content_stamp.generation_id(),
                published: &published,
            },
            &lease,
            || {
                if moves == 0 {
                    moves += 1;
                    git(
                        &worktree,
                        &[
                            "commit",
                            "-q",
                            "--allow-empty",
                            "-m",
                            "moved during capture",
                        ],
                    );
                }
            },
        )
        .unwrap();

        assert_eq!(moves, 1);
        assert_ne!(
            snapshot.stamp.unwrap().checkout_head,
            fixture.accepted_commit,
            "the settled snapshot names the head the capture ended on"
        );
    }

    #[test]
    fn a_checkout_that_never_settles_during_capture_is_transient() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);
        let published = accepted_knowledge_digests(&verified);
        let content_stamp = verified.content_stamp();
        let lease = server
            .acquire_catalog_overlay_lease(
                &fixture.project_id(),
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
            )
            .unwrap();

        // The working fingerprint changes on every read, so the two-read
        // agreement never holds inside the bounded window.
        let mut revision = 0;
        let error = stable_catalog_knowledge_overlay(
            CatalogOverlayPublished {
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
                write_entry(
                    &worktree,
                    &knowledge_entry("keep", &format!("edit {revision}")),
                );
            },
        )
        .expect_err("a checkout that keeps moving is busy, not positioned");
        assert_eq!(error.kind, OverlayRecomputeErrorKind::Transient);
        assert!(!error.is_structural());
    }

    #[test]
    fn a_detach_during_capture_fails_lease_revalidation() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);

        let degradation = server
            .compute_catalog_knowledge_overlay(
                &verified,
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
                || CatalogFixture::detach_in_server(&server, PEER_ATTACHMENT),
            )
            .err()
            .expect("bytes captured under a lease that no longer holds are unpublishable");
        assert_eq!(
            degradation.code,
            CheckoutAccessErrorCode::AttachmentInactive.as_str()
        );
        assert!(!degradation.transient);
    }

    #[test]
    fn an_advance_during_capture_refuses_the_snapshot() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "generation one")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_entry(&worktree, &knowledge_entry("keep", "checkout variant"));
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);

        let degradation = server
            .compute_catalog_knowledge_overlay(
                &verified,
                &attachment(PEER_ATTACHMENT, PEER_CHECKOUT),
                &fixture.scope,
                || {
                    fixture.catalog.install_publication(
                        PROJECT,
                        &fixture.scope,
                        &fixture.accepted_commit,
                        &[knowledge_entry("keep", "generation two")],
                        &[],
                    );
                    server.invalidate_catalog_published_content(&fixture.project_id());
                },
            )
            .expect_err("a snapshot may not position a checkout against unpublished bytes");
        assert_eq!(degradation.code, ERROR_OVERLAY_ACCEPTED_CONTENT_CHANGED);
        assert!(!degradation.transient);
    }

    /// Plan section 4.12: a checkout that cannot prove the baseline is a
    /// structural authority fact. Only a transient failure may hold a
    /// prior valid snapshot open.
    #[test]
    fn a_structural_failure_replaces_a_prior_snapshot_that_a_transient_one_preserves() {
        let fixture = OverlayFixture::new(&[knowledge_entry("keep", "accepted")]);
        let worktree = fixture.worktree("peer", PEER_ATTACHMENT, PEER_CHECKOUT);
        write_entry(&worktree, &knowledge_entry("keep", "checkout variant"));
        let server = fixture.catalog.server_with_checkout_authority();
        let verified = fixture.verified(&server);
        let peer = attachment(PEER_ATTACHMENT, PEER_CHECKOUT);

        let first = server
            .refresh_catalog_knowledge_overlay(&verified, &peer)
            .unwrap();
        assert_eq!(first.status, OverlayStatus::Valid);

        // A transient refusal keeps the prior valid snapshot open for a
        // bounded window: the checkout is busy, not wrong.
        let busy = server
            .state
            .checkout_access
            .lifecycle_mutation_guard()
            .unwrap();
        let preserved = server
            .refresh_catalog_knowledge_overlay(&verified, &peer)
            .unwrap();
        assert_eq!(preserved.status, OverlayStatus::Valid);
        assert_eq!(preserved.snapshot_id, first.snapshot_id);
        drop(busy);

        // The same checkout on an orphan branch still contains commit P
        // but shares no ancestor with it: no baseline exists, and that is
        // a fact about authority rather than retryable noise.
        git(&worktree, &["checkout", "-q", "--orphan", "orphaned"]);
        git(&worktree, &["add", ".bbox/knowledge"]);
        git(&worktree, &["commit", "-q", "-m", "orphan"]);

        let degradation = server
            .refresh_catalog_knowledge_overlay(&verified, &peer)
            .expect_err("a structural failure has no valid answer");
        assert_eq!(degradation.code, ERROR_OVERLAY_BASELINE_UNAVAILABLE);
        assert!(!degradation.transient);

        let stored = server
            .state
            .knowledge_overlays
            .read()
            .get(&fixture.scope, PEER_CHECKOUT)
            .cloned()
            .expect("the store keeps a record of the outcome");
        assert_eq!(
            stored.status,
            OverlayStatus::Invalid,
            "a structural failure must replace the prior snapshot, never preserve it"
        );
        assert!(stored.values.is_empty());
    }
}
