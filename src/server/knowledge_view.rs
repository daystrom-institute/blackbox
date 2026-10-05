use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bbox_corpus_core::built_from::{BuiltFromStamp, BuiltFromTable};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::accepted_publication_runtime::{
    AcceptedKnowledgeCategoryV1, AcceptedKnowledgeEntryV1, AcceptedKnowledgePriorityV1,
    AcceptedKnowledgeStatusV1, AcceptedPublicationContentStamp, AcceptedPublicationRuntimeError,
    AcceptedPublicationScopeAgreement, AcceptedPublicationSelection,
    ERROR_ACCEPTED_PUBLICATION_MISSING, VerifiedAcceptedPublication,
};
use bbox_knowledge::knowledge::{
    Category, Knowledge, KnowledgeEntry, KnowledgeViewMetadata, Priority, Scope, append_rationale,
    committed_knowledge_entry_bytes,
};
use bbox_knowledge::overlay::{
    PublishedKnowledgeEntry, PublishedKnowledgeSnapshot,
    load_published_snapshot_at_commit_unhydrated,
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
/// `built_from` stamp (legacy loaded state).
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
                    .filter(|entity_ref| entity_ref.starts_with("knowledge:"))
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
                let entity_ref = format!("knowledge:{id}");
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
        serde_json::json!({
            "rows": rows,
            "built_from": built_from,
            "diagnostics": &self.diagnostics,
        })
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

    /// The rendered diagnostics block. Filter and source diagnostics share
    /// one section.
    pub(crate) fn diagnostics_text(&self) -> Option<String> {
        (!self.diagnostics.is_empty()).then(|| {
            format!(
                "knowledge view degraded:\n- {}",
                self.diagnostics.join("\n- ")
            )
        })
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

    /// Materialize the exact published candidate set shared by list,
    /// search, inspection, and render consumers.
    pub(crate) fn session_knowledge_view(
        &self,
        requested_project: Option<&str>,
    ) -> Result<SessionKnowledgeView> {
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
                &mut items,
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
                metadata.insert(item.entry.id.clone(), item.metadata.clone());
                item.entry.clone()
            })
            .collect();
        // The detached view answers for the daemon's durable store: global
        // render authority judges that store's path, so an unbound (empty)
        // path would refuse every host-default global render.
        let source_store_path = self.state.kb.read().store_path().to_path_buf();
        Ok(SessionKnowledgeView {
            knowledge: Knowledge::detached_view(entries, metadata)
                .with_source_store_path(&source_store_path),
            items,
            built_from,
            diagnostics,
        })
    }

    /// Serve accepted published knowledge for every selected catalog
    /// project. Nothing here can fail the whole view: a project whose
    /// publication is missing, corrupt, or serving its prior generation
    /// degrades to a bounded diagnostic while its peers keep serving.
    fn append_catalog_published_knowledge(
        &self,
        requested_selector: Option<&str>,
        requested_project_id: Option<&str>,
        items: &mut BTreeMap<String, KnowledgeViewItem>,
        built_from: &mut BuiltFromTable,
        diagnostics: &mut Vec<String>,
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
        }
        Ok(())
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
        let snapshot = cached.unwrap_or_else(|| published_knowledge_from_accepted(verified));
        let mut queue = self.state.checkout_mutations.write();
        let is_current = self
            .state
            .accepted_publications
            .as_ref()
            .is_some_and(|runtime| {
                runtime
                    .load_verified(project_id)
                    .is_ok_and(|current| current.content_stamp() == content_stamp)
            });
        let paths = queue
            .outstanding_intents()
            .filter(|row| {
                is_current
                    && row.mutation.scope == snapshot.published_scope
                    && row.mutation.relative_path.starts_with(".bbox/knowledge/")
            })
            .map(|row| row.mutation.relative_path.clone())
            .collect::<BTreeSet<_>>();
        let mut changed = false;
        for path in paths {
            let id = path
                .trim_start_matches(".bbox/knowledge/")
                .trim_end_matches(".json");
            let content = snapshot
                .entries
                .get(id)
                .map(|entry| committed_knowledge_entry_bytes(&entry.entry))
                .transpose()
                .and_then(|bytes| bytes.map(String::from_utf8).transpose().map_err(Into::into));
            if let Ok(content) = content {
                changed |=
                    queue.observe_publication(&snapshot.published_scope, &path, content.as_deref());
            }
        }
        drop(queue);
        if changed {
            self.state.checkout_mutations_persister.request();
        }
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
    /// project's published view, enqueued on the single index writer. It
    /// reads accepted content only. Failure is
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
    ///
    /// The prior-arm read is refused rather than installed over a view that
    /// is already serving. A verified read falls back to the pointer's
    /// prior arm whenever the CURRENT generation does not verify, and it
    /// reports that only in its binding stamp: nothing about the value says
    /// "this is not the accepted generation". Knowledge and gaps survive
    /// that because they re-read on every request, so a transient
    /// current-arm failure heals itself. A graph view does not: installing
    /// prior-arm content latches the older generation into the read surface
    /// until the next accept or a daemon restart, which is exactly the
    /// silent staleness this path exists to prevent.
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
        if verified.binding_stamp().selection() == AcceptedPublicationSelection::Prior
            && self
                .state
                .project_graph_views
                .read()
                .published_view(project_id)
                .is_some()
        {
            // A project with no view at all is the one case where prior-arm
            // content is still an improvement, and the boot reconcile owns
            // it; here a view is already installed, so the honest move is to
            // leave it and say why.
            tracing::warn!(
                project_id = %project_id,
                accepted_generation = %verified.content_stamp().generation_id(),
                "published graph view refresh skipped: the current accepted generation did not \
                 verify and only the prior arm is readable; the installed view keeps serving"
            );
            return;
        }
        match bbox_indexing::project_graph_view::build_published_graph_view(&verified) {
            Ok(view) => {
                install_published_graph_view(
                    &self.state,
                    view,
                    PublishedGraphViewInstaller::AcceptRefresh,
                );
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
/// then swaps the catalog view under its own write guard.
///
/// `caller` names the install path in the refusal log. It is a fixed label,
/// never caller-supplied text.
pub(crate) fn install_published_graph_view(
    state: &SharedState,
    view: bbox_indexing::project_graph_view::PublishedProjectGraphView,
    caller: PublishedGraphViewInstaller,
) {
    if !published_graph_view_install_admitted(state, &view, caller) {
        return;
    }
    converge_published_graph_word_lanes(state, &view);
    state.project_graph_views.write().install_published(view);
}

/// Which path is installing a published graph view. Fixed labels, so a
/// refusal in the log says which trigger produced the losing view without
/// carrying caller-authored text into a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishedGraphViewInstaller {
    /// The convergence that follows an accepted-publication pointer move.
    AcceptRefresh,
    /// The boot pass over every published catalog project.
    BootReconcile,
    #[cfg(test)]
    Test,
}

impl PublishedGraphViewInstaller {
    fn as_str(self) -> &'static str {
        match self {
            Self::AcceptRefresh => "accept_refresh",
            Self::BootReconcile => "boot_reconcile",
            #[cfg(test)]
            Self::Test => "test",
        }
    }
}

/// The monotonic gate every published graph-view install passes through.
///
/// A published view is a projection of exactly ONE accepted generation, and
/// the read surface has no rebuild-on-read: whatever is installed keeps
/// answering. So an install that carries a generation the pointer no longer
/// names must not replace one that is serving, no matter which path built
/// it. The observed failure is a read that resolved accepted content, spent
/// real time while an acceptance landed, and then
/// installed its now-superseded view on top of the fresh one, leaving graph
/// reads answering from the previous generation until a restart.
///
/// The authority is the accepted publication POINTER, never the two views
/// being compared: generation ids are content digests with no ordering, so
/// "newer" can only mean "the generation the pointer currently names". That
/// also keeps the gate from latching: once a refresh arrives for the
/// pointer's generation it is admitted, even when the installed view is
/// stale.
///
/// Two degradations are admitted rather than refused, both loudly: nothing
/// is installed yet (the boot case, where prior-arm or superseded content
/// still beats no graphs at all), and an unreadable pointer with nothing
/// installed. An unreadable pointer with a view already serving keeps the
/// serving view: a gate that cannot prove supersession must not perform it.
pub(crate) fn published_graph_view_install_admitted(
    state: &SharedState,
    view: &bbox_indexing::project_graph_view::PublishedProjectGraphView,
    caller: PublishedGraphViewInstaller,
) -> bool {
    let project_id = &view.project_id;
    // Read the pointer BEFORE taking any view lock: this is file IO under
    // the publication lock, and the catalog view guard must never be held
    // across it.
    let accepted_generation = match &state.accepted_publications {
        Some(runtime) => match runtime.advance_tokens(project_id) {
            Ok(Some((generation, _pointer_sha256))) => Some(generation),
            // No pointer at all: nothing published, so there is no
            // authority to compare against and nothing to protect.
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    caller = caller.as_str(),
                    code = error.code(),
                    "published graph view install could not read the accepted pointer"
                );
                None
            }
        },
        // Bridge mode has no accepted-publication runtime and no accepted
        // pointer to be superseded by.
        None => return true,
    };
    if accepted_generation
        .as_deref()
        .is_some_and(|accepted| accepted == view.accepted_generation)
    {
        return true;
    }
    let installed = state
        .project_graph_views
        .read()
        .published_view(project_id)
        .map(|installed| installed.accepted_generation.clone());
    let Some(installed) = installed else {
        tracing::warn!(
            project_id = %project_id,
            caller = caller.as_str(),
            view_generation = %view.accepted_generation,
            accepted_generation = accepted_generation.as_deref().unwrap_or("<unreadable>"),
            "installing a published graph view the accepted pointer does not name, because no \
             view is serving yet"
        );
        return true;
    };
    tracing::warn!(
        project_id = %project_id,
        caller = caller.as_str(),
        view_generation = %view.accepted_generation,
        installed_generation = %installed,
        accepted_generation = accepted_generation.as_deref().unwrap_or("<unreadable>"),
        "published graph view install refused: it projects a generation the accepted pointer \
         does not name and a view is already serving"
    );
    false
}

/// Converge the published graph word lanes to one view WITHOUT touching the
/// in-memory catalog: the lane replacements and purges are computed and
/// enqueued here, and the caller performs the catalog swap under whatever
/// guard it needs.
///
/// Every graph in the view gets a whole-lane replacement keyed on its
/// generation stamp: same generation no-ops, a new generation rewrites the
/// lane, and a graph whose policy now disables text retrieval (or that left
/// the accepted view entirely) has its lane purged so its documents are
/// ABSENT from the index, not merely filtered out of one result list. Only
/// the published plane is indexed; the connector plane does not reach the
/// word index and must not piggyback on this path.
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
    converge_published_graph_vector_lane(state, view);
}

/// Converge the published graph VECTOR lane to one view (unified-retrieval
/// design 4.4): enqueue the composed embed projection of every
/// embed-eligible vertex (the queue dedups unchanged projections on the
/// versioned envelope hash, so a generation flip re-embeds only what
/// changed), and tombstone the vectors of vertices that were eligible under
/// the currently installed view but are not under this one: removed by the
/// flip, excluded by a policy change, their annotation withdrawn, or their
/// graph gone from the accepted view. Runs BEFORE the catalog swap so the
/// installed view is still the previous one. A boot install has no previous
/// view and tombstones nothing; the query-time authority hides any vector
/// that went stale while the daemon was down, and `bbox_reembed(route=graph)`
/// reconciles the partition exactly.
pub(crate) fn converge_published_graph_vector_lane(
    state: &SharedState,
    view: &bbox_indexing::project_graph_view::PublishedProjectGraphView,
) {
    let project_id = view.project_id.as_str().to_string();
    let previous: BTreeSet<String> = state
        .project_graph_views
        .read()
        .published_view(&view.project_id)
        .map(|installed| published_graph_embed_entity_ids(&project_id, installed).collect())
        .unwrap_or_default();
    let mut next = BTreeSet::new();
    let mut enqueued = 0usize;
    for (graph_id, entry) in &view.graphs {
        let Some(graph) = entry.graph() else {
            continue;
        };
        for projection in bbox_project_graph::graph_embed_projections(graph) {
            let entity_id = graph_vertex_entity_id(&project_id, graph_id, &projection.vertex_id);
            if crate::embed_queue::enqueue_graph_vertex(&project_id, &entity_id, &projection) {
                enqueued += 1;
            }
            next.insert(entity_id);
        }
    }
    let stale: Vec<String> = previous.difference(&next).cloned().collect();
    if !stale.is_empty() {
        crate::embed_queue::tombstone_graph_vertices(&stale);
    }
    if enqueued > 0 || !stale.is_empty() {
        tracing::debug!(
            project_id = %project_id,
            enqueued,
            tombstoned = stale.len(),
            "published graph vector lane converged"
        );
    }
}

/// Every embed-eligible vertex entity id of one published view.
pub(crate) fn published_graph_embed_entity_ids<'a>(
    project_id: &'a str,
    view: &'a bbox_indexing::project_graph_view::PublishedProjectGraphView,
) -> impl Iterator<Item = String> + 'a {
    view.graphs.iter().flat_map(move |(graph_id, entry)| {
        entry
            .graph()
            .map(|graph| bbox_project_graph::graph_embed_projections(graph))
            .unwrap_or_default()
            .into_iter()
            .map(move |projection| {
                graph_vertex_entity_id(project_id, graph_id, &projection.vertex_id)
            })
    })
}

fn graph_vertex_entity_id(project_id: &str, graph_id: &str, vertex_id: &str) -> String {
    bbox_corpus_core::entity_ref::EntityRef::ProjectGraphVertex {
        project_id: project_id.to_string(),
        graph_id: graph_id.to_string(),
        vertex_id: vertex_id.to_string(),
    }
    .to_string()
}

/// Reconcile the published graph views and their word lanes at boot
/// (unified-retrieval design 7.1).
///
/// `project_graph_views` is in-memory and only populated by accept-advance
/// and checkout-bind, but graph word-lane documents are durable. Without
/// this pass, a graph disabled or removed while the daemon was down stays
/// searchable with stale documents until the next install, and a schema
/// replacement (g13 drops the whole index) leaves the lanes empty until the
/// next accept. Driving one install per published catalog project at boot
/// closes both: the install's converge step purges lanes that left the
/// accepted view or lost text retrieval, and re-emits the rest. The lane
/// writes then settle asynchronously through the writer queue, so queries
/// that race ahead of the queue may briefly see the previous lane state.
/// Per-project failure degrades with a warning, matching the boot scan's
/// policy; bridge mode has no accepted-publication runtime and reconciles
/// nothing.
pub(crate) fn reconcile_published_graph_word_lanes_at_boot(
    state: &SharedState,
    projects: impl IntoIterator<Item = ProjectId>,
) {
    let Some(runtime) = &state.accepted_publications else {
        return;
    };
    let mut installed = 0usize;
    let mut skipped = 0usize;
    for project_id in projects {
        let verified = match runtime.load_verified(&project_id) {
            Ok(verified) => verified,
            Err(error) => {
                // The pre-bind scan already reported published-capability
                // damage per project; reconciling that project's lanes is
                // neither possible nor needed until its pointer recovers.
                tracing::debug!(
                    project_id = %project_id,
                    code = error.code(),
                    "boot graph view reconcile skipped: no verified accepted content"
                );
                skipped += 1;
                continue;
            }
        };
        if verified.binding_stamp().selection() == AcceptedPublicationSelection::Prior {
            // Boot is the one place prior-arm content is still worth
            // installing: no view exists yet, so the choice is the prior
            // generation or no graphs at all. It is a degradation either
            // way, and it stays installed until an accept refreshes it, so
            // it is said out loud rather than counted as a clean install.
            tracing::warn!(
                project_id = %project_id,
                accepted_generation = %verified.content_stamp().generation_id(),
                "boot graph view reconcile is serving the prior arm: the current accepted \
                 generation did not verify"
            );
        }
        match bbox_indexing::project_graph_view::build_published_graph_view(&verified) {
            Ok(view) => {
                install_published_graph_view(
                    state,
                    view,
                    PublishedGraphViewInstaller::BootReconcile,
                );
                installed += 1;
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "boot graph view reconcile failed; graph reads may serve stale content"
                );
                skipped += 1;
            }
        }
    }
    tracing::info!(
        installed,
        skipped,
        "published graph views reconciled at boot"
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
pub(crate) fn published_knowledge_from_accepted(
    verified: &VerifiedAcceptedPublication,
) -> PublishedKnowledgeSnapshot {
    let content_stamp = verified.content_stamp();
    let now = bbox_util::util::now_iso();
    let mut entries = BTreeMap::new();
    for manifest in verified.knowledge_manifest().values() {
        // Generation validation makes the manifest and the normalized
        // records a bijection, so a miss here is unreachable rather than a
        // silently dropped row.
        let Some(record) = verified.knowledge_records().get(&manifest.record_id) else {
            continue;
        };
        let Some(entry) = knowledge_entry_from_accepted(record, content_stamp.project_id(), &now)
        else {
            continue;
        };
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

/// Rebuild the domain entry from its accepted record, or `None` for a
/// retired record.
///
/// Version-1 rows carry fields the entry model no longer has. The same
/// legacy rules as stored entry files apply: a status other than active or
/// an expiry already past at `now` retires the row, a rationale is appended
/// to the content, and the `decision` category reads as `convention`. The
/// other legacy fields are ignored.
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
    now: &str,
) -> Option<KnowledgeEntry> {
    if record.status != AcceptedKnowledgeStatusV1::Active
        || record
            .expires_at
            .as_deref()
            .is_some_and(|expires| expires < now)
    {
        return None;
    }
    let content = match record.rationale.as_deref().map(str::trim) {
        Some(rationale) if !rationale.is_empty() && !record.content.contains(rationale) => {
            append_rationale(&record.content, rationale)
        }
        _ => record.content.clone(),
    };
    Some(KnowledgeEntry {
        render_placement: record.render_placement,
        id: record.id.as_str().to_string(),
        title: record.title.clone(),
        content,
        cluster: record.cluster.clone(),
        category: match record.category {
            AcceptedKnowledgeCategoryV1::Profile => Category::Profile,
            AcceptedKnowledgeCategoryV1::Convention => Category::Convention,
            AcceptedKnowledgeCategoryV1::Steering => Category::Steering,
            AcceptedKnowledgeCategoryV1::Build => Category::Build,
            AcceptedKnowledgeCategoryV1::Tool => Category::Tool,
            AcceptedKnowledgeCategoryV1::Memory => Category::Memory,
            AcceptedKnowledgeCategoryV1::Workflow => Category::Workflow,
            AcceptedKnowledgeCategoryV1::Decision => Category::Convention,
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
        render: record.render,
        created_at: record.created_at.clone(),
        updated_at: record.updated_at.clone(),
        recall_count: 0,
        last_recalled: None,
    })
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
                content_hash,
                built_from_ref: built_from_ref.map(str::to_owned),
                compatibility_lane: compatibility_lane.map(str::to_owned),
            },
            entity_ref,
            entry,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_knowledge::knowledge::{Category, Priority};
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

    /// Already-published version-1 rows keep their bytes; reading them
    /// applies the same legacy rules as stored entry files.
    #[test]
    fn published_version_one_rows_read_under_the_legacy_rules() {
        let row = |id: &str, category: &str, status: &str, expires_at: Option<&str>| {
            serde_json::from_value::<AcceptedKnowledgeEntryV1>(serde_json::json!({
                "id": id,
                "title": format!("row {id}"),
                "content": "the commitment",
                "cluster": null,
                "variants": {"claude": "claude-only"},
                "category": category,
                "scope": "project",
                "providers": [],
                "priority": "standard",
                "weight": 9,
                "status": status,
                "approval": "agent_inferred",
                "render": true,
                "decay": false,
                "review_at": null,
                "supersedes": "0000000000000000",
                "links": [{"target": "knowledge:x", "kind": "supports", "note": null, "source_arc": null, "confidence": "exact"}],
                "rationale": "the recorded reason",
                "expires_at": expires_at,
                "source": "agent",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-02T00:00:00Z"
            }))
            .unwrap()
        };
        let project = ProjectId::parse("p_legacy_rows").unwrap();
        let now = "2026-06-01T00:00:00Z";

        let decision = row("decision-row", "decision", "active", None);
        assert_eq!(decision.category, AcceptedKnowledgeCategoryV1::Decision);
        let entry = knowledge_entry_from_accepted(&decision, &project, now)
            .expect("an active version-1 row is an entry");
        assert_eq!(entry.category, Category::Convention);
        assert!(
            entry.render_placement.is_inline(),
            "a row normalized before placement was carried renders inline"
        );
        assert_eq!(
            entry.content,
            "the commitment\n\nRationale: the recorded reason"
        );
        assert_eq!(entry.project_id.as_deref(), Some("p_legacy_rows"));

        let convention = row("convention-row", "convention", "active", Some("2027-01-01"));
        let entry = knowledge_entry_from_accepted(&convention, &project, now).unwrap();
        assert_eq!(entry.category, Category::Convention);

        for status in ["superseded", "deleted", "draft", "disabled"] {
            assert!(
                knowledge_entry_from_accepted(
                    &row("retired", "convention", status, None),
                    &project,
                    now
                )
                .is_none(),
                "{status}"
            );
        }
        let expired = row(
            "expired",
            "convention",
            "active",
            Some("2026-01-01T00:00:00Z"),
        );
        assert!(knowledge_entry_from_accepted(&expired, &project, now).is_none());
    }

    fn entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            render_placement: Default::default(),
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            category: Category::Memory,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            render: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
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
        let aggregate = server.session_knowledge_view(None).unwrap();
        assert!(aggregate.knowledge.entry("first-legacy").is_some());
        assert!(aggregate.knowledge.entry("second-legacy").is_some());
        assert_eq!(
            aggregate.diagnostics,
            vec![compatibility_diagnostic.to_owned()]
        );

        let explicit = server
            .session_knowledge_view(Some(&first_record.canonical_path))
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

    #[tokio::test]
    async fn stale_knowledge_publication_read_cannot_retire_a_current_mutation() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_queue_stale", &scope);
        let project = ProjectId::parse("p_queue_stale").unwrap();
        let mut first = knowledge_entry("1234567890abcdef", "first");
        first.project_id = Some(project.as_str().into());
        fixture.install_publication(project.as_str(), &scope, COMMIT_ONE, &[first.clone()], &[]);
        let server = fixture.server();
        let runtime = server.state.accepted_publications.as_ref().unwrap();
        let stale = runtime.load_verified(&project).unwrap();
        let mut second = first.clone();
        second.content = "second".into();
        fixture.install_publication(project.as_str(), &scope, COMMIT_TWO, &[second.clone()], &[]);
        server.invalidate_catalog_published_content(&project);
        assert_ne!(
            runtime.load_verified(&project).unwrap().content_stamp(),
            stale.content_stamp()
        );
        let canonical = |entry: &KnowledgeEntry| {
            String::from_utf8(committed_knowledge_entry_bytes(entry).unwrap()).unwrap()
        };
        server
            .state
            .checkout_mutations
            .write()
            .enqueue_tracked_writes(
                scope.clone(),
                vec![(
                    format!(".bbox/knowledge/{}.json", first.id),
                    canonical(&first),
                    Some(canonical(&second)),
                )],
                "restore first".into(),
                "2026-09-06T00:00:00Z".into(),
            )
            .unwrap();
        server.cached_catalog_published_knowledge(&project, &stale);
        server.cached_catalog_published_knowledge(&project, &stale);
        assert_eq!(
            server
                .state
                .checkout_mutations
                .read()
                .outstanding_intents()
                .count(),
            1
        );
        fixture.install_publication(project.as_str(), &scope, &"3".repeat(40), &[first], &[]);
        server.invalidate_catalog_published_content(&project);
        server
            .session_knowledge_view(Some(project.as_str()))
            .unwrap();
        assert_eq!(
            server
                .state
                .checkout_mutations
                .read()
                .outstanding_intents()
                .count(),
            0
        );
    }

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
                .session_knowledge_view(None)
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
            .hybrid_word_lane_hits(
                &crate::index::HybridWordLane {
                    query,
                    limit: 5,
                    ..Default::default()
                },
                &view.active_selectors,
                &view.searcher,
            )
            .map(|hits| format!("{hits:?}"))
            .unwrap()
    }

    /// A session bound to a managed workspace reads exactly what an unbound
    /// session reads: the binding scopes render and write routing, never a
    /// read. Knowledge, gaps, and the default view all stay published.
    #[test]
    fn a_workspace_bound_session_reads_published_content() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project("p_bound", &scope);
        fixture.install_publication(
            "p_bound",
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "accepted content")],
            &[gap_note("gap-1234abcd", "accepted gap")],
        );
        let unbound = fixture.server();
        let bound = fixture.server();
        assert!(
            bound
                .session_workspace_binding
                .set(Some(Arc::new(
                    super::super::knowledge_source::WorkspaceBindingGrant {
                        task_id: "bound-task".into(),
                        session_id: "bound-session".into(),
                        project_id: "p_bound".into(),
                        scope: scope.clone(),
                        workspace_id: bro_core::WorkspaceId::parse("c".repeat(32)).unwrap(),
                        expires_unix_secs: u64::MAX,
                    },
                )))
                .is_ok()
        );
        let ids = |server: &BlackboxServer| {
            server
                .session_knowledge_view(None)
                .unwrap()
                .items
                .iter()
                .map(|item| (item.entity_ref.clone(), item.entry.content.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&bound), ids(&unbound));
        assert!(
            ids(&bound)
                .iter()
                .any(|(entity, content)| entity == "knowledge:knowledge-a"
                    && content == "accepted content")
        );
        let gaps = bound.session_gap_view(None).unwrap();
        assert_eq!(gaps.gaps.all().len(), 1);
        assert_eq!(gaps.gaps.all()[0].title, "accepted gap");
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

        let view = server.session_knowledge_view(None).unwrap();
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

        server.session_knowledge_view(None).unwrap();
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

        let after = server.session_knowledge_view(None).unwrap();
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

        let first = fixture.server().session_knowledge_view(None).unwrap();
        // A second server over the same durable bytes is a restart: new
        // runtime, empty caches, no attachment anywhere in the story.
        let second = fixture.server().session_knowledge_view(None).unwrap();
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

        let view = server.session_knowledge_view(None).unwrap();
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

        let view = server.session_knowledge_view(None).unwrap();
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

        let view = server.session_knowledge_view(None).unwrap();
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

        let view = server.session_knowledge_view(None).unwrap();
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

        server.session_knowledge_view(None).unwrap();
        let first_stamp = server
            .state
            .catalog_knowledge_published_cache
            .read()
            .get(&project_id)
            .expect("the first read installs a projected snapshot")
            .content_stamp
            .clone();
        server.session_knowledge_view(None).unwrap();
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
            row(&server.session_knowledge_view(None).unwrap(), "knowledge-a")
                .entry
                .content,
            "generation one"
        );
        server.invalidate_catalog_published_content(&project_id);
        let after = server.session_knowledge_view(None).unwrap();
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

        let view = server.session_knowledge_view(Some("p_first")).unwrap();
        assert_eq!(row(&view, "knowledge-a").entry.content, "first project");
        assert!(view.items.iter().all(|item| item.entry.id != "knowledge-b"));
    }
}
