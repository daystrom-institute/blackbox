use std::collections::{BTreeMap, BTreeSet};

use crate::knowledge::{
    DecideParams, ForgetParams, KnowledgeLinkParams, KnowledgeListParams, LearnParams,
    RememberParams, ResponseFormat,
};
use crate::packets::packet_matches_query;
use crate::server::BlackboxServer;
use crate::system_memory;

use anyhow::Context;
use bbox_indexing::accepted_publication_runtime::ERROR_ACCEPTED_PUBLICATION_MISSING;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::json;
use sha2::{Digest as _, Sha256};

const DEFAULT_PACKET_SIDECAR_LIMIT: usize = 8;
const DEFAULT_SYSTEM_MEMORY_SIDECAR_LIMIT: usize = 6;
const KNOWLEDGE_DETAIL_PAGE_BYTES: usize = 4096;
const KNOWLEDGE_DETAIL_MIN_PAGE_BYTES: usize = 256;
const STRUCTURED_KNOWLEDGE_CONTENT_BYTES: usize = 512;
const STRUCTURED_KNOWLEDGE_METADATA_BYTES: usize = 256;
const STRUCTURED_KNOWLEDGE_COLLECTION_BYTES: usize = 512;
const DIAGNOSTIC_PREVIEW_BYTES: usize = 160;
const DIAGNOSTIC_PREVIEW_COUNT: usize = 2;

#[derive(Clone, Copy)]
pub(crate) enum KnowledgeMutationOwner {
    Local,
    CheckoutQueue,
}

struct QueuedKnowledgeEdit<'a> {
    project_id: &'a str,
    scope: &'a bbox_corpus_core::identity::PublishedScope,
    published: BTreeMap<String, crate::knowledge::KnowledgeEntry>,
    queue: &'a mut crate::checkout_mutations::CheckoutMutations,
    changes: BTreeMap<String, Option<String>>,
}

impl QueuedKnowledgeEdit<'_> {
    fn published_content(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.published
            .get(id)
            .map(crate::knowledge::committed_knowledge_entry_bytes)
            .transpose()?
            .map(String::from_utf8)
            .transpose()
            .map_err(Into::into)
    }

    fn entry(&mut self, id: &str) -> anyhow::Result<crate::knowledge::KnowledgeEntry> {
        let id = id.strip_prefix("knowledge:").unwrap_or(id);
        let base = self.published_content(id)?;
        let content = self
            .queue
            .write_base(
                self.scope,
                &format!(".bbox/knowledge/{id}.json"),
                base.as_deref(),
            )?
            .ok_or_else(|| {
                anyhow::anyhow!("knowledge entry not found in the mutation scope: {id}")
            })?;
        let entry: crate::knowledge::KnowledgeEntry = serde_json::from_str(&content)?;
        anyhow::ensure!(
            entry.id == id
                && entry.scope == crate::knowledge::Scope::Project
                && entry.project_id.as_deref() == Some(self.project_id),
            "knowledge entry {id} does not belong to the mutation scope"
        );
        anyhow::ensure!(
            entry.status != crate::knowledge::Status::Deleted,
            "knowledge entry has been deleted: {id}"
        );
        Ok(entry)
    }

    fn stage(
        &mut self,
        entry: &crate::knowledge::KnowledgeEntry,
        delete: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            entry.scope == crate::knowledge::Scope::Project
                && entry.project_id.as_deref() == Some(self.project_id),
            "knowledge entry does not belong to the mutation scope"
        );
        let content = if delete {
            None
        } else {
            Some(String::from_utf8(
                crate::knowledge::committed_knowledge_entry_bytes(entry)?,
            )?)
        };
        anyhow::ensure!(
            !self.changes.contains_key(&entry.id),
            "a knowledge edit cannot target the same record twice"
        );
        self.changes.insert(entry.id.clone(), content);
        Ok(())
    }

    fn mint_id(&self) -> String {
        loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
            let path = format!(".bbox/knowledge/{id}.json");
            if !self.published.contains_key(&id)
                && !self.changes.contains_key(&id)
                && !self.queue.outstanding_intents().any(|row| {
                    &row.mutation.scope == self.scope && row.mutation.relative_path == path
                })
            {
                return id;
            }
        }
    }
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::knowledge_tools()
}

fn has_runtime_knowledge_filter(p: &KnowledgeListParams) -> bool {
    p.scope.is_some()
        || p.project.is_some()
        || p.provider.is_some()
        || p.status.is_some()
        || p.approval.is_some()
        || p.provisional.is_some()
}

/// Extract the top knowledge entry id from a `kb.list` entries block for the
/// response breadcrumb. The block opens with `N entries:\n\n[<id>] …`, so the
/// first bracketed token is the highest-ranked entry. Returns None for the
/// "No entries found." sentinel (no `[`), so a packet-only or memory-only
/// response does not emit a spurious entry pointer.
fn first_entry_id(entries_block: &str) -> Option<String> {
    let start = entries_block.find('[')? + 1;
    let end = entries_block[start..].find(']')? + start;
    let id = entries_block[start..end].trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn entry_ids(entries_block: &str) -> Vec<String> {
    entries_block
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('[')?;
            let end = rest.find(']')?;
            let id = rest[..end].trim();
            if id.is_empty() {
                return None;
            }
            match bbox_corpus_core::entity_ref::EntityRef::parse(id) {
                Ok(bbox_corpus_core::entity_ref::EntityRef::ProvisionalKnowledge {
                    entry_id,
                    ..
                }) => Some(entry_id),
                _ => Some(id.to_string()),
            }
        })
        .collect()
}

fn returned_entry_ids(entries_block: &str) -> Vec<String> {
    entries_block
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('[')?;
            let end = rest.find(']')?;
            let id = rest[..end].trim();
            (!id.is_empty()).then(|| id.to_string())
        })
        .collect()
}

fn knowledge_entity_ref(id: &str) -> String {
    if id.starts_with("provisional_knowledge:") {
        id.to_string()
    } else {
        format!("knowledge:{id}")
    }
}

fn stable_knowledge_overlay(
    publisher_root: &std::path::Path,
    published_ref: &str,
    checkout_lease: &bbox_indexing::checkout_access::ValidatedCheckoutLease,
    checkout: &bbox_corpus_core::project_record::ResolvedCheckoutScope,
) -> Result<bbox_knowledge::overlay::OverlaySnapshot, bbox_knowledge::overlay::OverlayRecomputeError>
{
    use bbox_knowledge::overlay::{
        OverlayRecomputeError, WorkingKnowledgeSnapshot, recompute_overlay_result,
    };

    let pending = || {
        checkout_lease
            .checkout_relative_regular_file_exists(
                ".bbox/local/knowledge-transactions/pending.json",
            )
            .map_err(anyhow::Error::new)
            .map_err(OverlayRecomputeError::transient)
    };
    let working = || {
        let files = checkout_lease
            .read_relative_json_directory(".bbox/knowledge")
            .map_err(anyhow::Error::new)
            .map_err(OverlayRecomputeError::transient)?;
        WorkingKnowledgeSnapshot::new(files).map_err(OverlayRecomputeError::transient)
    };
    if pending()? {
        return Err(OverlayRecomputeError::transient(anyhow::anyhow!(
            "checkout transaction is pending; provisional overlay refresh deferred"
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
            return Err(OverlayRecomputeError::transient(anyhow::anyhow!(
                "checkout transaction began during provisional overlay refresh"
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
        if same_knowledge_snapshot(&candidate, &next) && !pending()? {
            return Ok(next);
        }
        candidate = next;
    }
    Err(OverlayRecomputeError::transient(anyhow::anyhow!(
        "checkout state changed repeatedly during provisional overlay refresh"
    )))
}

fn classify_knowledge_overlay_access_error(
    error: anyhow::Error,
) -> bbox_knowledge::overlay::OverlayRecomputeError {
    if error
        .downcast_ref::<bbox_indexing::checkout_access::CheckoutAccessError>()
        .is_some_and(|access| {
            crate::server::checkout_access_error_is_definitively_stale(access.code)
        })
    {
        return bbox_knowledge::overlay::OverlayRecomputeError::invalid_content(error);
    }
    match error.downcast::<bbox_knowledge::overlay::OverlayRecomputeError>() {
        Ok(error) => error,
        Err(error) => bbox_knowledge::overlay::OverlayRecomputeError::transient(error),
    }
}

fn same_knowledge_snapshot(
    left: &bbox_knowledge::overlay::OverlaySnapshot,
    right: &bbox_knowledge::overlay::OverlaySnapshot,
) -> bool {
    left.snapshot_id == right.snapshot_id
        && left.status == right.status
        && left.diagnostics == right.diagnostics
}

fn log_tool_ok(tool: &'static str, start: std::time::Instant, bytes: usize) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes, "ok");
}

fn log_tool_err(tool: &'static str, start: std::time::Instant, err: &anyhow::Error) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %err, "err");
}

/// Rescope an absolute-path `project` filter through worktree→base project
/// resolution. A managed/linked worktree (or a subdirectory) of a registered
/// project hashes/keys differently from the base, so the literal path would
/// silently match nothing; entries live under the registered base path.
/// Rewrites `p.project` to the base canonical path and records the worktree
/// checkout root in `p.project_alias` so entries written from inside the
/// worktree (scoped to the worktree path) stay visible too. Non-path filters
/// (substring matches like "transcript-search") and unregistered paths are
/// left untouched.
///
/// Returns a diagnostic when the filter value named no registered project
/// (gap-40ab1102): that query keeps its literal substring semantics, and the
/// caller must be told so rather than reading an empty result as an empty
/// store.
fn rescope_project_filter(
    server: &crate::server::BlackboxServer,
    p: &mut KnowledgeListParams,
) -> Option<String> {
    use bbox_corpus_core::project_selector::{ProjectResolution, ResolvedAttachment};
    let raw = p.project.clone()?;
    // Identity arm (gap-40ab1102): a filter that resolves also arms the
    // dual-read id predicate, so rows stamped with a project_id match
    // whatever path key they carry. A catalog-published row carries no path
    // at all, which is how a path-free daemon answered a filtered query with
    // zero rows over entries that plainly held its project_id.
    // A blank value is the documented unscoped escape hatch, not a failed
    // resolution: it narrows nothing and reports nothing.
    let mut diagnostic = None;
    if p.project_id.is_none() && !raw.trim().is_empty() {
        match server.project_filter_identity(&raw) {
            Ok(project_id) => p.project_id = Some(project_id),
            Err(text) => diagnostic = Some(text),
        }
    }
    // Filter-class engine resolution (phase-2 §9.2): a selector that
    // resolves rewrites to the durable store key (worktree/subdir/alias/id →
    // registered base); one that does not keeps its substring-filter
    // semantics untouched. A worktree checkout is recorded in
    // `project_alias` so entries written from inside it stay visible.
    let Some(resolution) = server.resolve_project_filter(&raw) else {
        return diagnostic;
    };
    // Catalog-mode ledger arm (plan §8.2): path-only entries still keyed under
    // one of this project's historical paths stay visible after attachment
    // relocation stopped rewriting them. Empty in bridge mode.
    if let Some(project_id) = resolution.project_id() {
        p.project_ledger_paths = server.ledger_historical_paths(project_id);
    }
    let ProjectResolution::Attached(ctx) = resolution else {
        return diagnostic;
    };
    // The alias dir is where checkout-local rows land: the v1 checkout root,
    // or the catalog attachment's own project dir under the key-to-base rule.
    let checkout_dir = match &ctx.attachment {
        ResolvedAttachment::V1Compat { checkout_dir, .. } => checkout_dir.clone(),
        ResolvedAttachment::Catalog {
            checkout_project_dir,
            ..
        } => checkout_project_dir.clone(),
    };
    if checkout_dir != ctx.store_key {
        p.project_alias = Some(checkout_dir);
    }
    p.project = Some(ctx.store_key);
    diagnostic
}

impl BlackboxServer {
    fn guard_workspace_bound_project_knowledge(&self, scope: Option<&str>) -> anyhow::Result<()> {
        if self.authoritative_session_workspace_binding().is_some() && scope == Some("project") {
            anyhow::bail!(
                "error.knowledge_transport_authoritative: a bound workspace must write project knowledge through its checkout-owner transport"
            );
        }
        Ok(())
    }

    /// Rescope a knowledge WRITE's `project` param through worktree→base
    /// resolution: the entry's durable scope becomes the registered base
    /// (so render/list/inject filters keyed by the base path match it), and
    /// the returned write-dir — `Some` only for a recognized worktree —
    /// routes the repo-owned `.bbox/knowledge/` file into the caller's
    /// checkout so it travels with the branch (gap-de82a74d). Absence of
    /// `project` means GLOBAL write scope (tool-arg-defaulting §3.1) and is
    /// never touched; empty/whitespace values are left for store validation.
    fn prepare_knowledge_write(
        &self,
        project: &mut Option<String>,
        project_id: &mut Option<String>,
    ) -> anyhow::Result<(
        Option<bbox_knowledge::repo_io::KnowledgeRepoCarrier>,
        Option<bbox_corpus_core::project_record::ResolvedCheckoutScope>,
    )> {
        let Some(raw) = project.clone().filter(|s| !s.trim().is_empty()) else {
            return Ok((None, None));
        };
        // The coverage refusal must precede the checkout lease: acquiring a
        // RepositoryMutation lease canonicalizes the authority root, which a
        // zero-checkout-authority daemon cannot do, and the resulting
        // attachment_inactive would mask this lane's real guidance.
        if let Ok(project_id) = self.validate_project_selection(&raw)
            && self
                .state
                .knowledge_transport_cutover
                .covers_project_str(&project_id)
        {
            self.observe_knowledge_transport_operation(
                &project_id,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectKnowledgeMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::AuthoritativeRefusal,
            );
            anyhow::bail!(
                "error.knowledge_transport_authoritative: this project's knowledge is transport-governed and the daemon holds no checkout authority; write the entry as a committed .bbox/knowledge/ file on the checkout host and the collector will publish it"
            );
        }
        let resolution = self.resolve_project_write(&raw)?;
        let durable_scope = resolution.durable_scope;
        if let Some(project_id) = resolution.project_id.as_deref().filter(|project_id| {
            self.state
                .knowledge_transport_cutover
                .covers_project_str(project_id)
        }) {
            self.observe_knowledge_transport_operation(
                project_id,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectKnowledgeMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::AuthoritativeRefusal,
            );
            anyhow::bail!(
                "error.knowledge_transport_authoritative: covered project knowledge must be mutated by the checkout-owner harness"
            );
        }
        if let Some(project_id) = resolution.project_id.as_deref() {
            self.observe_knowledge_transport_operation(
                project_id,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectKnowledgeMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
            );
        }
        // Dual-read stamping (phase-2 §8.1): new rows carry the resolved
        // stable id beside the path key; the unregistered lane stays None.
        *project_id = resolution.project_id;
        let checkout = resolution.checkout_scope;
        let write_carrier = checkout
            .as_ref()
            .map(|checkout| {
                crate::server::repo_io::RepoIoAuthority::knowledge_checkout_carrier(
                    durable_scope.clone(),
                    checkout,
                )
            })
            .transpose()?;
        if self.path_fallback_is_cut() && checkout.is_none() {
            anyhow::bail!(
                "path-scoped project fallback is retired; project writes require a registered checkout with recorded repo identity"
            );
        }
        let registered_without_checkout = checkout.is_none()
            && self
                .state
                .records_provider
                .records_snapshot()
                .records
                .iter()
                .any(|record| record.canonical_path == durable_scope);
        if registered_without_checkout {
            anyhow::bail!(
                "managed checkout {raw} has no provisional identity; refusing a write that cannot be reconstructed after restart"
            );
        }
        *project = Some(durable_scope);
        if let Some(checkout) = checkout.as_ref() {
            self.register_dark_knowledge_checkout(checkout)?;
        }
        Ok((write_carrier, checkout))
    }

    fn mutate_queued_knowledge(
        &self,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
        reason: &str,
        edit: impl FnOnce(&mut QueuedKnowledgeEdit<'_>) -> anyhow::Result<String>,
    ) -> anyhow::Result<String> {
        self.mutate_queued_knowledge_with_snapshot_hook(project_id, scope, reason, || {}, edit)
    }

    fn mutate_queued_knowledge_with_snapshot_hook(
        &self,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
        reason: &str,
        mut after_snapshot: impl FnMut(),
        edit: impl FnOnce(&mut QueuedKnowledgeEdit<'_>) -> anyhow::Result<String>,
    ) -> anyhow::Result<String> {
        let project = bbox_corpus_core::project_catalog::ProjectId::parse(project_id)?;
        let runtime = self
            .state
            .accepted_publications
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("accepted publication runtime is unavailable"))?;
        let load = || match runtime.load_verified(&project) {
            Ok(current) => Ok(Some(current)),
            Err(error) if error.code() == ERROR_ACCEPTED_PUBLICATION_MISSING => Ok(None),
            Err(error) => Err(anyhow::Error::from(error)),
        };
        let mut captured = None;
        for _ in 0..4 {
            let verified = load()?;
            let published = verified
                .as_ref()
                .map(crate::server::knowledge_view::published_knowledge_from_accepted)
                .map(|snapshot| {
                    snapshot
                        .entries
                        .into_iter()
                        .map(|(id, entry)| (id, entry.entry))
                        .collect()
                })
                .unwrap_or_default();
            after_snapshot();
            let queue = self.state.checkout_mutations.write();
            anyhow::ensure!(
                self.covered_scope_for_project_id(project_id).as_ref() == Some(&scope),
                "project mutation scope changed; retry after scope reconciliation"
            );
            let current = load()?;
            if let Some(current) = &current {
                anyhow::ensure!(
                    current.content_stamp().accepted_scope() == &scope,
                    "project publication scope differs from the mutation target; retry after scope reconciliation"
                );
            }
            if verified.as_ref().map(|value| value.content_stamp())
                == current.as_ref().map(|value| value.content_stamp())
            {
                captured = Some((published, queue));
                break;
            }
        }
        let (published, mut queue) = captured.ok_or_else(|| {
            anyhow::anyhow!(
                "error.checkout_publication_busy: accepted publication changed repeatedly; retry the knowledge mutation"
            )
        })?;
        let mut transaction = QueuedKnowledgeEdit {
            project_id,
            scope: &scope,
            published,
            queue: &mut queue,
            changes: BTreeMap::new(),
        };
        let message = edit(&mut transaction)?;
        let mutations = transaction
            .changes
            .iter()
            .map(|(id, content)| {
                Ok((
                    format!(".bbox/knowledge/{id}.json"),
                    content.clone(),
                    transaction.published_content(id)?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mutation_ids = queue.enqueue_tracked_mutations(
            scope,
            mutations,
            reason.into(),
            bbox_util::util::now_iso(),
        )?;
        drop(queue);
        self.state.checkout_mutations_persister.request();
        self.observe_knowledge_transport_operation(
            project_id,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectKnowledgeMutation,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
        );
        if mutation_ids.is_empty() {
            Ok(message)
        } else {
            Ok(format!(
                "{message}; queued {}; delivery and publication are asynchronous",
                mutation_ids.join(", ")
            ))
        }
    }

    pub(crate) async fn persist_knowledge_mutation(
        &self,
        owner: KnowledgeMutationOwner,
    ) -> anyhow::Result<()> {
        match owner {
            KnowledgeMutationOwner::Local => self.state.kb_persister.request_durable().await,
            KnowledgeMutationOwner::CheckoutQueue => {
                self.state.persist_checkout_mutations_durable().await
            }
        }
    }

    pub(crate) async fn finish_knowledge_mutation(&self, result: CallToolResult) -> CallToolResult {
        if result.is_error != Some(true)
            && let Err(error) = self.state.persist_checkout_mutations_durable().await
        {
            return Self::err_text(&format!(
                "Error: knowledge changes were accepted, but checkout-queue durability failed: {error:#}"
            ));
        }
        result
    }

    /// Cosmetic title derivation for the checkout-owner lane (the store's
    /// own derive_title is crate-private): first content line, truncated.
    fn checkout_lane_title(content: &str, title: &Option<String>) -> String {
        title.clone().unwrap_or_else(|| {
            let first = content.lines().next().unwrap_or("").trim();
            let mut truncated: String = first.chars().take(72).collect();
            if first.chars().count() > 72 {
                truncated.push_str("...");
            }
            if truncated.is_empty() {
                "untitled".to_string()
            } else {
                truncated
            }
        })
    }

    /// Priority parse with the store's default (its parse_optional is
    /// crate-private; the enum's strum FromStr is not).
    fn checkout_lane_priority(raw: Option<&str>) -> anyhow::Result<crate::knowledge::Priority> {
        match raw {
            None => Ok(crate::knowledge::Priority::Standard),
            Some(value) => value
                .trim()
                .parse::<crate::knowledge::Priority>()
                .map_err(|_| anyhow::anyhow!("invalid priority: {value}")),
        }
    }

    fn covered_knowledge_entry_scope(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<(String, bbox_corpus_core::identity::PublishedScope)>> {
        let id = id.strip_prefix("knowledge:").unwrap_or(id);
        if self.state.project_authority.catalog_store().is_none() {
            return Ok(None);
        }
        let known_local = self.state.kb.read().all_entries().iter().any(|entry| {
            entry.id == id
                && (entry.scope == crate::knowledge::Scope::Global
                    || entry.project_id.as_deref().is_none_or(|project_id| {
                        !self
                            .state
                            .knowledge_transport_cutover
                            .covers_project_str(project_id)
                    }))
        });
        let mut owners = BTreeMap::new();
        for target in self.catalog_published_targets(None)? {
            let project_id = target.project_id.as_str();
            let Some(scope) = self.covered_scope_for_project_id(project_id) else {
                continue;
            };
            let runtime =
                self.state.accepted_publications.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("accepted publication runtime is unavailable")
                })?;
            let published = match runtime.load_verified(&target.project_id) {
                Ok(verified) => {
                    crate::server::knowledge_view::published_knowledge_from_accepted(&verified)
                        .entries
                        .contains_key(id)
                }
                Err(error) if error.code() == ERROR_ACCEPTED_PUBLICATION_MISSING => false,
                Err(_) if known_local => false,
                Err(error) => anyhow::bail!(
                    "cannot establish a unique knowledge owner while a project publication is unavailable; pass project to select the mutation owner: {error}"
                ),
            };
            let path = format!(".bbox/knowledge/{id}.json");
            let queued = self
                .state
                .checkout_mutations
                .read()
                .outstanding_intents()
                .any(|row| row.mutation.scope == scope && row.mutation.relative_path == path);
            if published || queued {
                owners.insert(project_id.to_string(), scope);
            }
        }
        anyhow::ensure!(
            owners.len() <= 1,
            "knowledge entry {id} exists in multiple projects; use a project-scoped mutation"
        );
        if !owners.is_empty() {
            anyhow::ensure!(
                !known_local,
                "knowledge entry {id} has multiple scope owners; use a project-scoped mutation"
            );
        }
        Ok(owners.into_iter().next())
    }

    fn covered_knowledge_mutation_scope(
        &self,
        id: &str,
        project: Option<&str>,
    ) -> anyhow::Result<Option<(String, bbox_corpus_core::identity::PublishedScope)>> {
        let Some(project) = project.filter(|project| !project.trim().is_empty()) else {
            return self.covered_knowledge_entry_scope(id);
        };
        let project_id = self.validate_project_selection(project)?;
        let scope = self.covered_scope_for_project_id(&project_id).ok_or_else(|| {
            anyhow::anyhow!(
                "project does not have checkout-owner mutation authority; omit project for global or local-store entries"
            )
        })?;
        Ok(Some((project_id, scope)))
    }

    /// Apply a learn write's field patch to a served entry (the store's
    /// update-branch semantics).
    fn patch_entry_from_learn(
        &self,
        entry: &mut crate::knowledge::KnowledgeEntry,
        p: &LearnParams,
    ) -> anyhow::Result<()> {
        let category = p
            .category
            .parse::<crate::knowledge::Category>()
            .map_err(|_| anyhow::anyhow!("invalid category: {}", p.category))?;
        entry.content = p.content.clone();
        entry.cluster = p
            .cluster
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        entry.title = Self::checkout_lane_title(&p.content, &p.title);
        entry.category = category;
        entry.priority = Self::checkout_lane_priority(p.priority.as_deref())?;
        entry.weight = p.weight.unwrap_or(100);
        entry.providers = p.providers.clone().unwrap_or_default();
        entry.updated_at = bbox_util::util::now_iso();
        if let Some(exp) = p.expires_at.clone() {
            entry.expires_at = Some(exp);
        }
        Ok(())
    }

    /// Covered-project learn update addressed purely by id (no project
    /// selector): coverage comes from the served entry itself.
    fn enqueue_learn_update_by_id_via_checkout_owner(
        &self,
        p: &LearnParams,
        id: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        let Some((project_id, scope)) = self.covered_knowledge_entry_scope(id)? else {
            return Ok(None);
        };
        let mut entry_id = String::new();
        let message =
            self.mutate_queued_knowledge(&project_id, scope, "bbox_learn update", |transaction| {
                let mut entry = transaction.entry(id)?;
                self.patch_entry_from_learn(&mut entry, p)?;
                transaction.stage(&entry, false)?;
                entry_id = entry.id.clone();
                Ok(format!("Updated entry {}", entry.id))
            })?;
        Ok(Some((message, entry_id)))
    }

    /// Covered-project review (approve/reject), id-addressed.
    pub(crate) fn enqueue_review_via_checkout_owner(
        &self,
        action: &str,
        id: &str,
        project: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let Some((project_id, scope)) = self.covered_knowledge_mutation_scope(id, project)? else {
            return Ok(None);
        };
        self.mutate_queued_knowledge(&project_id, scope, "bbox_review", |transaction| {
            let mut entry = transaction.entry(id)?;
            entry.updated_at = bbox_util::util::now_iso();
            let verb = match action {
                "approve" => {
                    entry.approval = crate::knowledge::Approval::UserConfirmed;
                    "Approved"
                }
                "reject" => "Rejected",
                other => anyhow::bail!("unknown review action {other}"),
            };
            transaction.stage(&entry, action == "reject")?;
            Ok(format!("{verb} entry {}", entry.id))
        })
        .map(Some)
    }

    /// Covered-project knowledge link: append the edge to the served
    /// source entry and enqueue its rewrite.
    fn enqueue_link_via_checkout_owner(
        &self,
        p: &KnowledgeLinkParams,
    ) -> anyhow::Result<Option<String>> {
        let source_id = match bbox_corpus_core::entity_ref::EntityRef::parse(&p.source) {
            Ok(bbox_corpus_core::entity_ref::EntityRef::Knowledge { id }) => id,
            Ok(other) => anyhow::bail!("source must be a knowledge ref, got {other}"),
            Err(_) => p.source.trim_start_matches("knowledge:").to_string(),
        };
        let Some((project_id, scope)) =
            self.covered_knowledge_mutation_scope(&source_id, p.project.as_deref())?
        else {
            return Ok(None);
        };
        bbox_corpus_core::entity_ref::EntityRef::parse(&p.target)
            .map_err(|err| anyhow::anyhow!("target must be a valid entity ref: {err}"))?;
        let kind = crate::knowledge::KnowledgeEdgeKind::parse(&p.kind)?;
        let confidence = match p.confidence.as_deref().unwrap_or("heuristic") {
            "exact" | "Exact" | "EXACT" => bbox_chunker::EdgeConfidence::Exact,
            "heuristic" | "Heuristic" | "HEURISTIC" => bbox_chunker::EdgeConfidence::Heuristic,
            "unknown" | "Unknown" | "UNKNOWN" => bbox_chunker::EdgeConfidence::Unknown,
            other => anyhow::bail!(
                "invalid edge confidence '{other}' (expected exact, heuristic, or unknown)"
            ),
        };
        let edge = crate::knowledge::KnowledgeEdge {
            target: p.target.clone(),
            kind,
            note: p.note.clone(),
            source_arc: p.source_arc.clone(),
            confidence,
        };
        self.mutate_queued_knowledge(&project_id, scope, "bbox_knowledge_link", |transaction| {
            let mut entry = transaction.entry(&source_id)?;
            let duplicate = entry.links.iter().any(|existing| {
                existing.target == edge.target
                    && existing.kind == edge.kind
                    && existing.source_arc == edge.source_arc
            });
            if duplicate {
                return Ok(format!(
                    "Link already present on {} (same target, kind, and arc)",
                    entry.id
                ));
            }
            entry.links.push(edge);
            entry.updated_at = bbox_util::util::now_iso();
            transaction.stage(&entry, false)?;
            Ok(format!("Linked {} -> {}", entry.id, p.target))
        })
        .map(Some)
    }

    /// Covered-project learn: create or update through the checkout-owner
    /// backchannel, mirroring the store's field semantics.
    fn enqueue_learn_via_checkout_owner(
        &self,
        p: &LearnParams,
        _raw: &str,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
    ) -> anyhow::Result<String> {
        let category = p
            .category
            .parse::<crate::knowledge::Category>()
            .map_err(|_| anyhow::anyhow!("invalid category: {}", p.category))?;
        let title = Self::checkout_lane_title(&p.content, &p.title);
        let priority = Self::checkout_lane_priority(p.priority.as_deref())?;
        let weight = p.weight.unwrap_or(100);
        let cluster = p
            .cluster
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let providers = p.providers.clone().unwrap_or_default();
        let now = bbox_util::util::now_iso();
        if let Some(id) = p.id.as_deref() {
            return self.mutate_queued_knowledge(
                project_id,
                scope,
                "bbox_learn update",
                |transaction| {
                    let mut entry = transaction.entry(id)?;
                    self.patch_entry_from_learn(&mut entry, p)?;
                    transaction.stage(&entry, false)?;
                    Ok(format!("Updated entry {}", entry.id))
                },
            );
        }
        let entry = crate::knowledge::KnowledgeEntry {
            id: String::new(),
            title,
            content: p.content.clone(),
            cluster,
            variants: std::collections::HashMap::new(),
            category,
            scope: crate::knowledge::Scope::Project,
            project: None,
            project_id: Some(project_id.to_string()),
            providers,
            priority,
            weight,
            render: true,
            decay: true,
            review_at: None,
            status: crate::knowledge::Status::Active,
            approval: crate::knowledge::Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: p.expires_at.clone(),
            source: "user".to_string(),
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        };
        self.mutate_queued_knowledge(project_id, scope, "bbox_learn create", |transaction| {
            let entry = crate::knowledge::KnowledgeEntry {
                id: transaction.mint_id(),
                ..entry
            };
            transaction.stage(&entry, false)?;
            Ok(format!("Created entry {} [render_pending=true]", entry.id))
        })
    }

    /// Covered-project remember: create an indexed-only entry through the
    /// backchannel.
    fn enqueue_remember_via_checkout_owner(
        &self,
        p: &RememberParams,
        _raw: &str,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
    ) -> anyhow::Result<String> {
        let category = match p.category.as_deref() {
            None => crate::knowledge::Category::Memory,
            Some(raw_category) => raw_category
                .parse::<crate::knowledge::Category>()
                .map_err(|_| anyhow::anyhow!("invalid category: {raw_category}"))?,
        };
        let now = bbox_util::util::now_iso();
        let entry = crate::knowledge::KnowledgeEntry {
            id: String::new(),
            title: Self::checkout_lane_title(&p.content, &p.title),
            content: p.content.clone(),
            cluster: None,
            variants: std::collections::HashMap::new(),
            category,
            scope: crate::knowledge::Scope::Project,
            project: None,
            project_id: Some(project_id.to_string()),
            providers: Vec::new(),
            priority: crate::knowledge::Priority::Standard,
            weight: 100,
            render: false,
            decay: p.decay.unwrap_or(true),
            review_at: p.review_at.clone(),
            status: crate::knowledge::Status::Active,
            approval: crate::knowledge::Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: p.expires_at.clone(),
            source: "user".to_string(),
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        };
        self.mutate_queued_knowledge(project_id, scope, "bbox_remember", |transaction| {
            let entry = crate::knowledge::KnowledgeEntry {
                id: transaction.mint_id(),
                ..entry
            };
            transaction.stage(&entry, false)?;
            Ok(format!(
                "Remembered entry {} (indexed only, not rendered)",
                entry.id
            ))
        })
    }

    /// Covered-project decide: create the decision and, when superseding,
    /// enqueue the predecessor's superseded rewrite too.
    fn enqueue_decide_via_checkout_owner(
        &self,
        p: &DecideParams,
        _raw: &str,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
    ) -> anyhow::Result<String> {
        if p.content.trim().is_empty() {
            anyhow::bail!("'content' is required");
        }
        if p.rationale.trim().is_empty() {
            anyhow::bail!(
                "'rationale' is required: a decision without justification is just a command"
            );
        }
        let priority = Self::checkout_lane_priority(p.priority.as_deref())?;
        let now = bbox_util::util::now_iso();
        let entry = crate::knowledge::KnowledgeEntry {
            id: String::new(),
            title: Self::checkout_lane_title(&p.content, &p.title),
            content: p.content.clone(),
            cluster: None,
            variants: std::collections::HashMap::new(),
            category: crate::knowledge::Category::Decision,
            scope: crate::knowledge::Scope::Project,
            project: None,
            project_id: Some(project_id.to_string()),
            providers: Vec::new(),
            priority,
            weight: 100,
            render: p.render.unwrap_or(true),
            decay: false,
            review_at: None,
            status: crate::knowledge::Status::Active,
            approval: crate::knowledge::Approval::UserConfirmed,
            supersedes: None,
            links: Vec::new(),
            rationale: Some(p.rationale.clone()),
            expires_at: None,
            source: "user".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
            recall_count: 0,
            last_recalled: None,
        };
        self.mutate_queued_knowledge(project_id, scope, "bbox_decide", |transaction| {
            let entry = crate::knowledge::KnowledgeEntry {
                id: transaction.mint_id(),
                ..entry
            };
            let mut message = format!("Decided entry {}", entry.id);
            if let Some(old_id) = p.supersedes.as_deref() {
                let mut old = transaction.entry(old_id)?;
                old.status = crate::knowledge::Status::Superseded;
                old.supersedes = Some(entry.id.clone());
                old.updated_at = now;
                transaction.stage(&old, false)?;
                message.push_str(&format!("; superseded {}", old.id));
            }
            transaction.stage(&entry, false)?;
            Ok(message)
        })
    }

    /// Covered-project forget, id-addressed: coverage comes from the
    /// served entry's stamped project id, not a selector. Supersede
    /// rewrites the record; a plain forget deletes the file. Entries the
    /// published view does not serve (legacy central rows) fall through to
    /// the store path unchanged.
    fn enqueue_forget_via_checkout_owner(
        &self,
        p: &crate::knowledge::ForgetParams,
    ) -> anyhow::Result<Option<String>> {
        let id = p.id.strip_prefix("knowledge:").unwrap_or(&p.id);
        let Some((project_id, scope)) =
            self.covered_knowledge_mutation_scope(id, p.project.as_deref())?
        else {
            return Ok(None);
        };
        self.mutate_queued_knowledge(&project_id, scope, "bbox_forget", |transaction| {
            let mut entry = transaction.entry(id)?;
            entry.updated_at = bbox_util::util::now_iso();
            if let Some(by) = p.superseded_by.as_deref() {
                entry.status = crate::knowledge::Status::Superseded;
                entry.supersedes = Some(by.to_string());
                transaction.stage(&entry, false)?;
                return Ok(format!("Superseded entry {}", entry.id));
            }
            transaction.stage(&entry, true)?;
            Ok(format!("Removed entry {}", entry.id))
        })
        .map(Some)
    }

    pub(crate) fn register_dark_knowledge_checkout(
        &self,
        checkout: &bbox_corpus_core::project_record::ResolvedCheckoutScope,
    ) -> anyhow::Result<()> {
        self.register_checkout_row(
            bbox_indexing::checkout_registry::CheckoutRow {
                project_id: Some(checkout.project_id.clone()),
                checkout_id: checkout.checkout_id.clone(),
                checkout_dir: checkout.checkout_dir.clone(),
                repo_id: Some(checkout.published_scope.repo_id().to_string()),
                bbox_root_relpath: Some(checkout.published_scope.bbox_root_relpath().to_string()),
                branch_ref: checkout.branch_ref.clone(),
            },
            None,
        )?;
        self.watch_resolved_dark_knowledge_checkout(checkout);
        Ok(())
    }

    pub(crate) fn register_checkout_row(
        &self,
        row: bbox_indexing::checkout_registry::CheckoutRow,
        mut write_lease: Option<&mut bbox_indexing::checkout_access::ValidatedCheckoutLease>,
    ) -> anyhow::Result<bbox_indexing::checkout_registry::RegistrationOutcome> {
        use bbox_indexing::checkout_registry::{
            CheckoutRegistrationApplyError, CheckoutRegistrationPreflight, CheckoutRegistryChanged,
            RegistrationOutcome,
        };

        for _ in 0..3 {
            let preflight = {
                self.state
                    .checkout_registry
                    .write()
                    .preflight_register(row.clone())?
            };
            let token = match preflight {
                CheckoutRegistrationPreflight::Unchanged => {
                    return Ok(RegistrationOutcome::Unchanged);
                }
                CheckoutRegistrationPreflight::ChangeRequired(token) => token,
            };
            // Preflight's registry/store locks are gone before this bounded
            // lifecycle wait. Changed commits take lifecycle first, registry
            // second; an exact repeat never enters this branch.
            let lifecycle = match write_lease.take() {
                Some(lease) => self
                    .state
                    .checkout_access
                    .promote_write_lease_to_lifecycle_guard(lease),
                None => self.state.checkout_access.lifecycle_mutation_guard(),
            }
            .map_err(anyhow::Error::new)?;
            let applied = self
                .state
                .checkout_registry
                .write()
                .compare_and_register(token);
            drop(lifecycle);
            match applied {
                Ok(outcome) => return Ok(outcome),
                Err(CheckoutRegistrationApplyError::CheckoutRegistryChanged(_)) => continue,
                Err(CheckoutRegistrationApplyError::Store(error)) => return Err(error),
            }
        }
        Err(anyhow::Error::new(CheckoutRegistryChanged))
    }

    pub(crate) fn refresh_dark_knowledge_overlay(
        &self,
        checkout: &bbox_corpus_core::project_record::ResolvedCheckoutScope,
    ) -> crate::server::KnowledgeOverlayRefreshOutcome {
        use crate::server::KnowledgeOverlayRefreshOutcome;
        use bbox_knowledge::overlay::{
            OverlayKey, OverlayRecomputeError, OverlayRecomputeErrorKind, OverlaySnapshot,
            OverlayStatus, TransientPreservationOutcome,
        };

        let _refresh = self.state.knowledge_overlay_refresh.lock();
        // An explicit overlay refresh is an observer boundary, not a hot view
        // read. Re-resolve authority now so publisher movement promotes away
        // matching provisional values immediately. The following gap refresh
        // reuses this freshly cached decision.
        self.invalidate_publisher_authority_cache(&checkout.published_scope);
        let generation = self
            .state
            .knowledge_overlays
            .write()
            .begin_refresh(OverlayKey {
                published_scope: checkout.published_scope.clone(),
                checkout_id: checkout.checkout_id.clone(),
            });
        let projects = self.state.records_provider.records_snapshot().records;
        let prior = self
            .state
            .knowledge_overlays
            .read()
            .get(&checkout.published_scope, &checkout.checkout_id)
            .cloned();
        let prior_is_valid = prior
            .as_ref()
            .is_some_and(|snapshot| snapshot.status == OverlayStatus::Valid);
        let mut publisher_project = None;
        let mut publication_guard = None;
        let snapshot = match self
            .authorize_publisher_classified(&projects, &checkout.published_scope)
        {
            Ok(publisher) => {
                publisher_project = projects
                    .iter()
                    .find(|project| project.project_id == publisher.project_id)
                    .map(|project| project.canonical_path.clone());
                let refreshed = match self.acquire_authorized_overlay_access(&publisher, checkout) {
                    Ok((publisher_lease, checkout_lease)) => {
                        let prepared = stable_knowledge_overlay(
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
                            Err(error) => {
                                Err(OverlayRecomputeError::transient(anyhow::Error::new(error)))
                            }
                        }
                    }
                    Err(error) => Err(classify_knowledge_overlay_access_error(error)),
                };
                match refreshed {
                    Ok(snapshot) => snapshot,
                    Err(err)
                        if err.kind == OverlayRecomputeErrorKind::Transient && prior_is_valid =>
                    {
                        tracing::warn!(
                            error = %err,
                            checkout = %checkout.checkout_id,
                            scope = ?checkout.published_scope,
                            "knowledge overlay refresh degraded; preserving prior valid snapshot"
                        );
                        let mut preserved = prior.clone().expect("prior valid snapshot");
                        preserved.diagnostics = vec![format!("refresh degraded: {err:#}")];
                        match self
                            .state
                            .knowledge_overlays
                            .write()
                            .preserve_transient_if_latest(generation, preserved)
                        {
                            TransientPreservationOutcome::Preserved { .. } => {
                                return KnowledgeOverlayRefreshOutcome::PreservedTransient;
                            }
                            TransientPreservationOutcome::Superseded => {
                                return KnowledgeOverlayRefreshOutcome::Superseded;
                            }
                            TransientPreservationOutcome::Exhausted => OverlaySnapshot::invalid(
                                checkout,
                                format!(
                                    "transient knowledge overlay refresh limit exceeded: {err:#}"
                                ),
                            ),
                        }
                    }
                    Err(err) => OverlaySnapshot::invalid(checkout, format!("{err:#}")),
                }
            }
            Err(err) if err.is_transient() && prior_is_valid => {
                tracing::warn!(
                    error = %err,
                    checkout = %checkout.checkout_id,
                    scope = ?checkout.published_scope,
                    "knowledge publisher refresh degraded; preserving prior valid snapshot"
                );
                let mut preserved = prior.clone().expect("prior valid snapshot");
                preserved.diagnostics = vec![format!("publisher refresh degraded: {err:#}")];
                match self
                    .state
                    .knowledge_overlays
                    .write()
                    .preserve_transient_if_latest(generation, preserved)
                {
                    TransientPreservationOutcome::Preserved { .. } => {
                        return KnowledgeOverlayRefreshOutcome::PreservedTransient;
                    }
                    TransientPreservationOutcome::Superseded => {
                        return KnowledgeOverlayRefreshOutcome::Superseded;
                    }
                    TransientPreservationOutcome::Exhausted => OverlaySnapshot::invalid(
                        checkout,
                        format!("transient knowledge publisher refresh limit exceeded: {err:#}"),
                    ),
                }
            }
            Err(err) => OverlaySnapshot::invalid(checkout, format!("{err:#}")),
        };
        let _publication_is_held = publication_guard.as_ref();
        let invalid = snapshot.status == OverlayStatus::Invalid;
        let prior_commit = prior
            .as_ref()
            .and_then(|snapshot| snapshot.stamp.as_ref())
            .map(|stamp| stamp.publisher_commit.as_str());
        let current_commit = snapshot
            .stamp
            .as_ref()
            .map(|stamp| stamp.publisher_commit.as_str());
        let publisher_moved = prior_commit != current_commit;
        let unchanged = prior.as_ref().is_some_and(|prior| {
            prior.snapshot_id == snapshot.snapshot_id
                && prior.status == snapshot.status
                && prior.diagnostics == snapshot.diagnostics
        });
        let mut affected = BTreeSet::new();
        if let Some(prior) = &prior {
            affected.extend(prior.values.keys().cloned());
        }
        affected.extend(snapshot.values.keys().cloned());

        if !self
            .state
            .knowledge_overlays
            .write()
            .publish_if_latest(generation, snapshot)
        {
            return KnowledgeOverlayRefreshOutcome::Superseded;
        }
        if unchanged && !invalid {
            return KnowledgeOverlayRefreshOutcome::Converged;
        }
        self.invalidate_published_snapshot_caches(&checkout.published_scope);

        let Some(project) = publisher_project else {
            self.clear_knowledge_scope_in_index(&checkout.published_scope);
            return KnowledgeOverlayRefreshOutcome::Invalid;
        };
        if invalid {
            if let Err(err) =
                self.sync_knowledge_scope_to_index(&checkout.published_scope, &project)
            {
                tracing::warn!(
                    error = %err,
                    scope = ?checkout.published_scope,
                    "invalid knowledge overlay convergence failed closed"
                );
                self.clear_knowledge_scope_in_index(&checkout.published_scope);
            }
            return KnowledgeOverlayRefreshOutcome::Invalid;
        }
        if publisher_moved {
            if let Err(err) =
                self.sync_knowledge_scope_to_index(&checkout.published_scope, &project)
            {
                tracing::warn!(
                    error = %err,
                    scope = ?checkout.published_scope,
                    "knowledge publisher convergence failed closed"
                );
                self.clear_knowledge_scope_in_index(&checkout.published_scope);
            }
            return KnowledgeOverlayRefreshOutcome::Converged;
        }
        for entry_id in affected {
            if let Err(err) = self.sync_knowledge_logical_ref_for_project(&entry_id, &project) {
                tracing::warn!(
                    error = %err,
                    entry = %entry_id,
                    "knowledge overlay convergence failed; the next observer pass will retry"
                );
            }
        }
        KnowledgeOverlayRefreshOutcome::Converged
    }
}

fn matches_system_memory_catalog(category: Option<&str>) -> bool {
    matches!(
        category,
        Some("system_memory") | Some("system-memory") | Some("system_memories")
    )
}

fn format_system_memory_catalog(query: Option<&str>) -> String {
    system_memory::format_catalog_summary(query)
}

fn system_memory_catalog_response(p: &KnowledgeListParams) -> Option<String> {
    if has_runtime_knowledge_filter(p) {
        return None;
    }
    if matches_system_memory_catalog(p.category.as_deref()) {
        return Some(format_system_memory_catalog(p.query.as_deref()));
    }
    None
}

fn exact_system_memory_target(
    p: &KnowledgeListParams,
) -> Option<&'static system_memory::SystemMemory> {
    if has_runtime_knowledge_filter(p) || p.category.is_some() {
        return None;
    }
    system_memory::exact_query(p.query.as_deref())
}

fn exact_system_memory_response(p: &KnowledgeListParams) -> Option<String> {
    if let Some(memory) = exact_system_memory_target(p) {
        let mut out = String::new();
        out.push_str("── System memories ──────────────────────────\n");
        out.push_str(&system_memory::format_for_listing(memory));
        return Some(out);
    }
    system_memory_catalog_response(p)
}

fn knowledge_detail_page_limit(requested: Option<u64>) -> usize {
    requested
        .map(|limit| limit as usize)
        .unwrap_or(KNOWLEDGE_DETAIL_PAGE_BYTES)
        .clamp(KNOWLEDGE_DETAIL_MIN_PAGE_BYTES, KNOWLEDGE_DETAIL_PAGE_BYTES)
}

fn projection_revision(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn parse_projection_cursor(cursor: &str, revision: &str) -> anyhow::Result<usize> {
    let (cursor_revision, offset) = cursor.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("invalid detail cursor: expected <content_sha256>:<offset>")
    })?;
    anyhow::ensure!(
        cursor_revision == revision,
        "stale detail cursor: the underlying content changed; restart the same detail read without detail_cursor"
    );
    offset
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("invalid detail cursor offset"))
}

fn projection_body_page(
    subject: &str,
    scope_seed: &str,
    body: &str,
    format: &str,
    cursor: Option<&str>,
    requested_limit: Option<u64>,
) -> anyhow::Result<serde_json::Value> {
    let revision = projection_revision(&[subject, scope_seed, body]);
    let offset = match cursor {
        Some(cursor) => parse_projection_cursor(cursor, &revision)?,
        None => 0,
    };
    anyhow::ensure!(
        offset <= body.len(),
        "stale detail cursor: page offset {offset} is past the {} byte body; restart without detail_cursor",
        body.len()
    );
    anyhow::ensure!(
        body.is_char_boundary(offset),
        "invalid detail cursor: offset splits a Unicode character"
    );
    let limit = knowledge_detail_page_limit(requested_limit);
    let mut end = (offset + limit).min(body.len());
    while end > offset && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut page = json!({
        "text": &body[offset..end],
        "format": format,
        "offset": offset,
        "end": end,
        "total_bytes": body.len(),
        "complete": end == body.len(),
        "content_sha256": revision,
    });
    if end < body.len() {
        page["next_cursor"] = json!(format!("{revision}:{end}"));
    }
    Ok(page)
}

fn detail_text_marker(kind: &str, page: &serde_json::Value) -> String {
    let offset = page["offset"].as_u64().unwrap_or_default();
    let end = page["end"].as_u64().unwrap_or_default();
    let total = page["total_bytes"].as_u64().unwrap_or_default();
    let mut out = format!(
        "Exact {kind} body page {offset}..{end} of {total} bytes is in structuredContent.body; "
    );
    if page["complete"].as_bool() == Some(true) {
        out.push_str("this page is complete.");
    } else {
        out.push_str("continue with the returned body.next_cursor in detail_cursor.");
    }
    out
}

fn knowledge_query_scope(p: &KnowledgeListParams) -> serde_json::Value {
    json!({
        "category": p.category,
        "scope": p.scope,
        "project": p.project,
        "provider": p.provider,
        "status": p.status,
        "approval": p.approval,
        "query": p.query,
        "mode": p.mode,
        "provisional": p.provisional,
    })
}

fn knowledge_recovery_arguments(p: &KnowledgeListParams, entity_ref: &str) -> serde_json::Value {
    let mut arguments = knowledge_query_scope(p);
    arguments["entry_detail"] = json!(entity_ref);
    if let Some(limit) = p.detail_limit {
        arguments["detail_limit"] = json!(limit);
    }
    arguments
}

fn resolve_knowledge_item<'a>(
    items: &'a [crate::server::knowledge_view::KnowledgeViewItem],
    selector: &str,
) -> anyhow::Result<&'a crate::server::knowledge_view::KnowledgeViewItem> {
    let selector = selector.trim();
    anyhow::ensure!(
        !selector.is_empty(),
        "entry_detail must name a knowledge entry"
    );
    if selector.starts_with("knowledge:") || selector.starts_with("provisional_knowledge:") {
        return items
            .iter()
            .find(|item| item.entity_ref == selector)
            .with_context(|| format!("knowledge variant {selector} is not visible in this view"));
    }
    let matches: Vec<_> = items
        .iter()
        .filter(|item| item.entry.id == selector || item.entity_ref == selector)
        .collect();
    match matches.as_slice() {
        [item] => Ok(item),
        [] => anyhow::bail!("knowledge entry {selector} is not visible in this view"),
        _ => {
            let refs = matches
                .iter()
                .map(|item| item.entity_ref.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "knowledge id {selector} is ambiguous across variants; pass one canonical entity_ref: {refs}"
            );
        }
    }
}

fn knowledge_row_ref(item: &crate::server::knowledge_view::KnowledgeViewItem) -> String {
    item.entity_ref.clone()
}

fn diagnostic_recovery_arguments(p: &KnowledgeListParams) -> serde_json::Value {
    let mut arguments = knowledge_query_scope(p);
    arguments["diagnostics_detail"] = json!(true);
    if let Some(limit) = p.detail_limit {
        arguments["detail_limit"] = json!(limit);
    }
    arguments
}

fn compact_text_fragment(text: &str, limit: usize) -> String {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let fragment = &text[..end];
    if fragment.len() == text.len() {
        fragment.to_string()
    } else {
        format!("{fragment}...")
    }
}

fn diagnostic_state(diagnostic: &str) -> &'static str {
    let diagnostic = diagnostic.to_ascii_lowercase();
    if diagnostic.contains("unavailable") || diagnostic.contains("missing") {
        "unavailable"
    } else if diagnostic.contains("stale") || diagnostic.contains("changed") {
        "stale"
    } else if diagnostic.contains("queued") || diagnostic.contains("pending") {
        "queued"
    } else if diagnostic.contains("partial") || diagnostic.contains("incomplete") {
        "partial"
    } else if diagnostic.contains("degraded") || diagnostic.contains("skipped") {
        "degraded"
    } else if diagnostic.contains("filter") || diagnostic.contains("registered project") {
        "filter"
    } else {
        "other"
    }
}

fn diagnostic_summary(diagnostics: &[String], p: &KnowledgeListParams) -> serde_json::Value {
    let mut states = BTreeMap::<&str, usize>::new();
    for diagnostic in diagnostics {
        *states.entry(diagnostic_state(diagnostic)).or_default() += 1;
    }
    json!({
        "count": diagnostics.len(),
        "states": states,
        "previews": diagnostics
            .iter()
            .take(DIAGNOSTIC_PREVIEW_COUNT)
            .map(|diagnostic| compact_text_fragment(diagnostic, DIAGNOSTIC_PREVIEW_BYTES))
            .collect::<Vec<_>>(),
        "preview_count": DIAGNOSTIC_PREVIEW_COUNT.min(diagnostics.len()),
        "omitted_bytes": diagnostics
            .iter()
            .map(|diagnostic| diagnostic.len())
            .sum::<usize>(),
        "recovery": (!diagnostics.is_empty()).then(|| json!({
            "tool": "bbox_knowledge",
            "arguments": diagnostic_recovery_arguments(p),
            "preserves_query_scope": true,
            "live_view": true,
            "changed_behavior": "Diagnostics are recomputed for the same filters; changed content invalidates detail_cursor.",
        })),
    })
}

fn diagnostic_summary_text(summary: &serde_json::Value) -> String {
    let count = summary["count"].as_u64().unwrap_or_default();
    if count == 0 {
        return String::new();
    }
    let states = summary["states"]
        .as_object()
        .map(|states| {
            states
                .iter()
                .map(|(state, count)| format!("{state}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    format!(
        "[knowledge visibility: {count} diagnostics ({states}); compact previews and exact recovery are in structuredContent.diagnostics]"
    )
}

fn exact_diagnostics_response(
    diagnostics: &[String],
    p: &KnowledgeListParams,
) -> anyhow::Result<(String, serde_json::Value)> {
    let body = serde_json::to_string(diagnostics)?;
    let scope = knowledge_query_scope(p).to_string();
    let page = projection_body_page(
        "bbox_knowledge diagnostics",
        &scope,
        &body,
        "json",
        p.detail_cursor.as_deref(),
        p.detail_limit,
    )?;
    let text = detail_text_marker("diagnostics", &page);
    let structured = json!({
        "detail": "diagnostics",
        "scope": knowledge_query_scope(p),
        "diagnostic_count": diagnostics.len(),
        "body": page,
        "provenance": {
            "source": "session knowledge visibility for the request scope",
            "format": "json",
            "live_view": true,
        },
    });
    Ok((text, structured))
}

fn exact_system_memory_detail_response(
    memory: &system_memory::SystemMemory,
    p: &KnowledgeListParams,
) -> anyhow::Result<(String, serde_json::Value)> {
    let body = serde_json::to_string(
        &json!({"id": memory.id, "title": memory.title, "tags": memory.tags, "content": memory.content}),
    )?;
    let page = projection_body_page(
        "system memory record",
        &memory.id,
        &body,
        "json",
        p.detail_cursor.as_deref(),
        p.detail_limit,
    )?;
    let text = detail_text_marker("system memory", &page);
    let structured = json!({
        "detail": "system_memory",
        "entity_ref": format!("system_memory:{}", memory.id),
        "body": page,
        "provenance": {
            "source": "file-loaded system memory catalog",
            "format": "json",
            "content_sha256": page["content_sha256"],
            "live_view": false,
        },
    });
    Ok((text, structured))
}

fn exact_entry_detail_response(
    view: &crate::server::knowledge_view::SessionKnowledgeView,
    p: &KnowledgeListParams,
    visible_refs: &[String],
) -> anyhow::Result<(String, serde_json::Value)> {
    let selector = p.entry_detail.as_deref().unwrap_or_default();
    let item = resolve_knowledge_item(&view.items, selector)?;
    let row_ref = knowledge_row_ref(item);
    anyhow::ensure!(
        visible_refs.contains(&row_ref),
        "knowledge variant {} is not in the requested filter scope",
        item.entity_ref
    );
    let mut canonical_entry = item.entry.clone();
    if item.entity_ref.starts_with("provisional_knowledge:") {
        canonical_entry.id = item.entity_ref.clone();
    }
    let body = serde_json::to_string(&canonical_entry)?;
    let scope_seed = format!(
        "{}\0{}",
        item.entity_ref,
        knowledge_query_scope(p).to_string()
    );
    let page = projection_body_page(
        "knowledge entry record",
        &scope_seed,
        &body,
        "json",
        p.detail_cursor.as_deref(),
        p.detail_limit,
    )?;
    let text = detail_text_marker("knowledge entry", &page);
    let structured = json!({
        "detail": "knowledge_entry",
        "entity_ref": item.entity_ref,
        "scope": knowledge_query_scope(p),
        "record": {
            "id": item.entry.id,
            "entity_ref": item.entity_ref,
            "category": format!("{:?}", item.entry.category),
            "content_preview": compact_text_fragment(&item.entry.content, DIAGNOSTIC_PREVIEW_BYTES),
            "content_bytes": item.entry.content.len(),
        },
        "body": page,
        "provenance": {
            "source": "session knowledge view",
            "format": "json",
            "content_sha256": page["content_sha256"],
            "live_view": true,
        },
    });
    Ok((text, structured))
}

fn bound_entry_metadata(entry: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    let mut truncated = false;
    for field in [
        "title",
        "cluster",
        "project",
        "project_id",
        "supersedes",
        "rationale",
        "expires_at",
        "source",
    ] {
        let Some(value) = entry
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if value.len() > STRUCTURED_KNOWLEDGE_METADATA_BYTES {
            let bytes_field = format!("{field}_bytes");
            entry.insert(
                field.into(),
                json!(compact_text_fragment(
                    &value,
                    STRUCTURED_KNOWLEDGE_METADATA_BYTES
                )),
            );
            entry.insert(bytes_field, json!(value.len()));
            truncated = true;
        }
    }
    for field in ["variants", "providers", "links"] {
        let Some(value) = entry.get(field).cloned() else {
            continue;
        };
        let serialized = serde_json::to_string(&value).unwrap_or_default();
        if serialized.len() > STRUCTURED_KNOWLEDGE_COLLECTION_BYTES {
            let count = match &value {
                serde_json::Value::Object(map) => map.len(),
                serde_json::Value::Array(rows) => rows.len(),
                _ => 0,
            };
            entry[field] = json!({
                "count": count,
                "bytes": serialized.len(),
                "truncated": true,
            });
            truncated = true;
        }
    }
    truncated
}

fn bound_structured_knowledge_rows(structured: &mut serde_json::Value, p: &KnowledgeListParams) {
    let Some(rows) = structured["rows"].as_array_mut() else {
        return;
    };
    for row in rows {
        let Some(entry) = row["entry"].as_object_mut() else {
            continue;
        };
        let mut truncated = false;
        if let Some(content) = entry
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            && content.len() > STRUCTURED_KNOWLEDGE_CONTENT_BYTES
        {
            entry["content"] = json!(compact_text_fragment(
                &content,
                STRUCTURED_KNOWLEDGE_CONTENT_BYTES
            ));
            entry.insert("content_bytes".into(), json!(content.len()));
            entry.insert("content_truncated".into(), json!(true));
            truncated = true;
        }
        truncated |= bound_entry_metadata(entry);
        let Some(entity_ref) = row["entity_ref"].as_str().map(str::to_string) else {
            continue;
        };
        if !truncated {
            continue;
        }
        let arguments = knowledge_recovery_arguments(p, &entity_ref);
        row["detail"] = json!({
            "tool": "bbox_knowledge",
            "arguments": arguments,
            "live_view": true,
            "changed_behavior": "A changed entry or filter scope invalidates detail_cursor.",
        });
    }
}

fn validate_knowledge_detail_selection(
    p: &KnowledgeListParams,
    exact_memory_requested: bool,
) -> anyhow::Result<()> {
    let diagnostics = p.diagnostics_detail == Some(true);
    let entry = p
        .entry_detail
        .as_deref()
        .is_some_and(|selector| !selector.trim().is_empty());
    let selected =
        usize::from(diagnostics) + usize::from(entry) + usize::from(exact_memory_requested);
    anyhow::ensure!(
        selected <= 1,
        "choose only one detail mode: diagnostics_detail, entry_detail, or an exact sm-* query"
    );
    if p.entry_detail
        .as_deref()
        .is_some_and(|selector| selector.trim().is_empty())
    {
        anyhow::bail!("entry_detail must not be empty");
    }
    if (p.detail_cursor.is_some() || p.detail_limit.is_some()) && selected == 0 {
        anyhow::bail!(
            "detail_cursor and detail_limit require diagnostics_detail, entry_detail, or an exact sm-* query"
        );
    }
    Ok(())
}

#[tool_router(router = knowledge_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_learn",
        description = "Persist an operator-approved rule or convention that should bind future sessions; rendered into provider markdown files. Use for narrative rules (\"we always X\", \"never Y\") only after the operator has approved the exact content and scope. If the rule you're storing is actually a priority-ordered decision function, classification rubric, or structured mechanism, use `bbox_compile` instead; that produces a shareable packet any agent can apply deterministically."
    )]
    pub(crate) async fn bbox_learn(
        &self,
        Parameters(p): Parameters<LearnParams>,
    ) -> CallToolResult {
        if let Err(error) = self.guard_workspace_bound_project_knowledge(p.scope.as_deref()) {
            return Self::err_text(&format!("Error: {error:#}"));
        }
        let format = match ResponseFormat::parse_optional(p.format.as_deref()) {
            Ok(format) => format,
            Err(e) => return Self::err_text(&format!("Error: {e:#}")),
        };
        let warning = self.arc_bound_warning(p.id.as_deref(), &p.content);
        let start = std::time::Instant::now();
        let server = self.clone();
        // Covered projects write through the checkout-owner backchannel:
        // the daemon validates and enqueues the exact committed-file bytes
        // and the collector applies them; a zero-authority daemon has no
        // local checkout to lease.
        let covered = p
            .project
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|raw| {
                self.covered_project_scope(raw)
                    .map(|(project_id, scope)| (raw.to_string(), project_id, scope))
            });
        if let Some((raw, project_id, scope)) = covered {
            let delivered = tokio::task::spawn_blocking(move || {
                server.enqueue_learn_via_checkout_owner(&p, &raw, &project_id, scope)
            })
            .await
            .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
            .and_then(std::convert::identity);
            return self
                .finish_knowledge_mutation(match delivered {
                    Ok(message) => Self::ok_text(&message),
                    Err(e) => Self::err_text(&format!("Error: {e:#}")),
                })
                .await;
        }
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            // Update-by-id without an explicit project selector: coverage
            // comes from the served entry itself.
            if p.project.as_deref().is_none_or(|s| s.trim().is_empty())
                && let Some(existing_ref) = p.id.as_deref()
                && let Some((message, entry_id)) =
                    server.enqueue_learn_update_by_id_via_checkout_owner(&p, existing_ref)?
            {
                return Ok::<_, anyhow::Error>((
                    crate::knowledge::LearnWriteResult {
                        id: entry_id,
                        action: "updated".to_string(),
                        rendered: false,
                        render_pending: true,
                        summary: None,
                        message,
                    },
                    None,
                    true,
                    KnowledgeMutationOwner::CheckoutQueue,
                ));
            }
            let update = match p.id.as_deref() {
                Some(existing_ref) => {
                    let existing = server.prepare_existing_knowledge_mutation(existing_ref)?;
                    if p.project
                        .as_deref()
                        .is_none_or(|project| project.trim().is_empty())
                    {
                        p.project = existing
                            .seed
                            .as_ref()
                            .and_then(|entry| entry.project.clone());
                    }
                    p.id = Some(existing.id.clone());
                    Some(existing)
                }
                None => None,
            };
            let (write_dir, checkout) =
                server.prepare_knowledge_write(&mut p.project, &mut p.project_id)?;
            if let Some(update) = &update
                && (update.carrier.as_ref() != write_dir.as_ref()
                    || update.checkout.as_ref().map(|scope| &scope.checkout_id)
                        != checkout.as_ref().map(|scope| &scope.checkout_id))
            {
                anyhow::bail!(
                    "an updated knowledge entry must use the same checkout authority as the write"
                );
            }
            let mut kb = server.state.kb.write();
            let result = kb.learn_result_with_checkout(
                &p,
                false,
                write_dir
                    .as_ref()
                    .map(|carrier| carrier.carrier_id.as_str()),
                update.as_ref().and_then(|update| update.seed.as_ref()),
            )?;
            let rider = kb.repo_record_rider_at(&result.id, write_dir.as_ref())?;
            drop(kb);
            let overlay_refreshed = checkout.is_some();
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_knowledge_overlay(checkout);
            }
            Ok::<_, anyhow::Error>((
                result,
                rider,
                overlay_refreshed,
                KnowledgeMutationOwner::Local,
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider, overlay_refreshed, owner)) => {
                if let Err(e) = self.persist_knowledge_mutation(owner).await {
                    log_tool_err("bbox_learn", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if !overlay_refreshed
                    && let Err(err) = self.sync_knowledge_entry_to_index(&result.id)
                {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                match format {
                    ResponseFormat::Text => {
                        let mut text = match warning {
                            Some(w) => format!("{}{}", result.message, w),
                            None => result.message,
                        };
                        if let Some(rider) = &rider {
                            text.push_str(rider);
                        }
                        log_tool_ok("bbox_learn", start, text.len());
                        Self::ok_text(&text)
                    }
                    ResponseFormat::Json => {
                        let message = match &rider {
                            Some(rider) => format!("{}{}", result.message, rider),
                            None => result.message,
                        };
                        let mut payload = serde_json::json!({
                            "id": result.id,
                            "action": result.action,
                            "rendered": result.rendered,
                            "render_pending": result.render_pending,
                            "message": message,
                        });
                        if let Some(summary) = result.summary {
                            payload["summary"] = serde_json::json!(summary);
                        }
                        if let Some(w) = warning {
                            payload["warnings"] = serde_json::json!([w.trim().to_string()]);
                        }
                        let bytes = serde_json::to_string(&payload)
                            .map(|s| s.len())
                            .unwrap_or_default();
                        log_tool_ok("bbox_learn", start, bytes);
                        Self::ok_json(&payload)
                    }
                }
            }
            Err(e) => {
                log_tool_err("bbox_learn", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_remember",
        description = "Persist a fact for later recall; indexed but NOT rendered."
    )]
    pub(crate) async fn bbox_remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> CallToolResult {
        if let Err(error) = self.guard_workspace_bound_project_knowledge(p.scope.as_deref()) {
            return Self::err_text(&format!("Error: {error:#}"));
        }
        let start = std::time::Instant::now();
        let server = self.clone();
        let covered = p
            .project
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|raw| {
                self.covered_project_scope(raw)
                    .map(|(project_id, scope)| (raw.to_string(), project_id, scope))
            });
        if let Some((raw, project_id, scope)) = covered {
            let delivered = tokio::task::spawn_blocking(move || {
                server.enqueue_remember_via_checkout_owner(&p, &raw, &project_id, scope)
            })
            .await
            .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
            .and_then(std::convert::identity);
            return self
                .finish_knowledge_mutation(match delivered {
                    Ok(message) => Self::ok_text(&message),
                    Err(e) => Self::err_text(&format!("Error: {e:#}")),
                })
                .await;
        }
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            let (write_dir, checkout) =
                server.prepare_knowledge_write(&mut p.project, &mut p.project_id)?;
            let mut kb = server.state.kb.write();
            let result = kb.remember_result_with_write_dir(
                &p,
                false,
                write_dir
                    .as_ref()
                    .map(|carrier| carrier.carrier_id.as_str()),
            )?;
            let rider = kb.repo_record_rider_at(&result.id, write_dir.as_ref())?;
            drop(kb);
            let overlay_refreshed = checkout.is_some();
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_knowledge_overlay(checkout);
            }
            Ok::<_, anyhow::Error>((result, rider, overlay_refreshed))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider, overlay_refreshed)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_remember", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if !overlay_refreshed
                    && let Err(err) = self.sync_knowledge_entry_to_index(&result.id)
                {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                log_tool_ok("bbox_remember", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_remember", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_decide",
        description = "Record a durable commitment with required rationale; supports supersession."
    )]
    pub(crate) async fn bbox_decide(
        &self,
        Parameters(p): Parameters<DecideParams>,
    ) -> CallToolResult {
        if let Err(error) = self.guard_workspace_bound_project_knowledge(p.scope.as_deref()) {
            return Self::err_text(&format!("Error: {error:#}"));
        }
        let start = std::time::Instant::now();
        let server = self.clone();
        let covered = p
            .project
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .and_then(|raw| {
                self.covered_project_scope(raw)
                    .map(|(project_id, scope)| (raw.to_string(), project_id, scope))
            });
        if let Some((raw, project_id, scope)) = covered {
            let delivered = tokio::task::spawn_blocking(move || {
                server.enqueue_decide_via_checkout_owner(&p, &raw, &project_id, scope)
            })
            .await
            .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
            .and_then(std::convert::identity);
            return self
                .finish_knowledge_mutation(match delivered {
                    Ok(message) => Self::ok_text(&message),
                    Err(e) => Self::err_text(&format!("Error: {e:#}")),
                })
                .await;
        }
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            let (write_dir, checkout) = server.prepare_knowledge_write(&mut p.project, &mut p.project_id)?;
            let superseded = match p.supersedes.as_deref() {
                Some(old_ref) => {
                    let existing = server.prepare_existing_knowledge_mutation(old_ref)?;
                    if existing.carrier.as_ref() != write_dir.as_ref()
                        || existing.checkout.as_ref().map(|scope| &scope.checkout_id)
                            != checkout.as_ref().map(|scope| &scope.checkout_id)
                    {
                        anyhow::bail!(
                            "a superseding decision and its predecessor must use the same checkout authority"
                        );
                    }
                    p.supersedes = Some(existing.id);
                    existing.seed
                }
                None => None,
            };
            let mut kb = server.state.kb.write();
            let result = kb.decide_result_with_checkout(
                &p,
                false,
                write_dir
                    .as_ref()
                    .map(|carrier| carrier.carrier_id.as_str()),
                superseded.as_ref(),
            )?;
            let rider = kb.repo_record_rider_at(&result.id, write_dir.as_ref())?;
            drop(kb);
            let overlay_refreshed = checkout.is_some();
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_knowledge_overlay(checkout);
            }
            Ok::<_, anyhow::Error>((result, rider, overlay_refreshed))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider, overlay_refreshed)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_decide", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if !overlay_refreshed
                    && let Err(err) = self.sync_knowledge_entry_to_index(&result.id)
                {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                if !overlay_refreshed && let Some(old_id) = result.superseded.as_deref() {
                    if let Err(err) = self.tombstone_knowledge_entry_in_index(old_id) {
                        tracing::warn!(error = %err, entry = %old_id, "knowledge index tombstone failed; will reconstruct on next reindex cycle");
                    }
                }
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                log_tool_ok("bbox_decide", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_decide", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_knowledge",
        description = "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces bounded rule-packet and system-memory sidecars; system memories include system_memory:<id> refs usable with bbox_inspect_entity or bbox_bundle_evidence. Pass category=\"packet\" to list compiled packets, category=\"system_memory\" to list memory metadata, or bbox_packet_list for structured packet filters."
    )]
    pub(crate) async fn bbox_knowledge(
        &self,
        Parameters(p): Parameters<KnowledgeListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_knowledge", move || {
            let exact_memory = exact_system_memory_target(&p);
            validate_knowledge_detail_selection(&p, exact_memory.is_some())?;
            if let Some(out) = system_memory_catalog_response(&p) {
                return Ok((out.clone(), json!({ "text": out })));
            }
            if let Some(memory) = exact_memory {
                return exact_system_memory_detail_response(memory, &p);
            }

            let mut p = p;
            let mut filter_diagnostics = Vec::new();
            if p.project.is_some()
                && let Some(diagnostic) = rescope_project_filter(&server, &mut p)
            {
                filter_diagnostics.push(diagnostic);
            }
            // The mutation store's reload tolerates unreadable carriers by
            // skipping them; tell the caller which projects that hid.
            for (project, error) in server.state.kb.read().degraded_carriers() {
                if !server.carrier_affects_project_filter(project, p.project.as_deref(), p.project_id.as_deref(), &p.project_ledger_paths) {
                    continue;
                }
                filter_diagnostics.push(format!(
                    "knowledge store overlay skipped for project {project}: {error}"
                ));
            }

            let mut view = server.session_knowledge_view(
                p.project.as_deref(),
                p.provisional.as_deref(),
            )?;
            if p.entry_detail.is_some() {
                let mut scope_params = p.clone();
                scope_params.limit = Some(u64::MAX);
                let scoped = view.knowledge.list(&scope_params)?;
                let visible_refs = returned_entry_ids(&scoped)
                    .iter()
                    .map(|id| knowledge_entity_ref(id))
                    .collect::<Vec<_>>();
                return exact_entry_detail_response(&view, &p, &visible_refs);
            }
            let mut combined = view.knowledge.list(&p)?;
            let returned_ids = returned_entry_ids(&combined);
            // Response-scoped diagnostics (gap-40ab1102): the legacy-lane
            // line belongs to the rows this caller actually got, and the
            // filter-resolution lines lead so an empty result explains
            // itself.
            let returned_legacy_rows = view.returned_rows_include_legacy_lane(&returned_ids);
            view.finalize_response_diagnostics(returned_legacy_rows, filter_diagnostics);
            if p.diagnostics_detail == Some(true) {
                return exact_diagnostics_response(&view.diagnostics, &p);
            }
            // Captured before packets/memories are appended, so it reflects the
            // top knowledge entry (not a packet/memory line).
            let top_entry_id = first_entry_id(&combined);
            let recall_ids = entry_ids(&combined);
            if !recall_ids.is_empty() {
                let recall_result = {
                    let mut kb = server.state.kb.write();
                    kb.record_recall(&recall_ids)
                };
                match recall_result {
                    Ok(()) => {
                        // Recall telemetry was always best-effort on this read path;
                        // keep it write-behind rather than making queries wait for fsync.
                        server.state.kb_persister.request();
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "knowledge recall telemetry update failed");
                    }
                }
            }

            // Surface matching packets. Uses the same match semantics as
            // bbox_packet_list so the two tools agree on what "matches" means.
            let all_packets = server.state.packets.read().list_all()?;
            let matching_packets: Vec<_> =
                if let Some(q) = p.query.as_deref().filter(|q| !q.is_empty()) {
                    all_packets
                        .into_iter()
                        .filter(|pkt| packet_matches_query(pkt, q))
                        .collect()
                } else if p.category.as_deref() == Some("packet") {
                    all_packets
                } else {
                    Vec::new()
                };

            if !matching_packets.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Rule-packets ───────────────────────────────\n");
                let limit = p
                    .limit
                    .map(|limit| limit as usize)
                    .unwrap_or(DEFAULT_PACKET_SIDECAR_LIMIT)
                    .min(25);
                for pkt in matching_packets.iter().take(limit) {
                    let histogram: Vec<String> = pkt
                        .rules
                        .iter()
                        .fold(BTreeMap::<String, usize>::new(), |mut acc, r| {
                            *acc.entry(r.classification.clone()).or_insert(0) += 1;
                            acc
                        })
                        .into_iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect();
                    combined.push_str(&format!(
                        "[{}] Packet | domain: {} | scope: {} | {} rules [{}] | created {}\n",
                        pkt.id,
                        pkt.domain,
                        pkt.scope,
                        pkt.rules.len(),
                        histogram.join(", "),
                        pkt.created_at,
                    ));
                }
                if matching_packets.len() > limit {
                    combined.push_str(&format!(
                        "  [truncated rule-packets: showing {limit} of {}; use bbox_packet_list for structured filters]\n",
                        matching_packets.len()
                    ));
                }
                combined.push_str(
                    "  (use bbox_packet_list for filter/query/preview; bbox_apply to evaluate)\n",
                );
            }

            // Also surface matching system memories. See
            // system-defaults/memories/ — these are file-loaded runbooks
            // read at startup, queryable but never rendered.
            //
            // The broad query path renders compact signposts, not full bodies:
            // a fuzzy multi-term query matches many runbooks, and full bodies
            // (~40KB each) overflow the token budget. The agent recovers a
            // body through bounded pages by querying its exact sm-* id.
            let memories = system_memory::search(p.query.as_deref());
            if !memories.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── System memories ──────────────────────────\n");
                let limit = p
                    .limit
                    .map(|limit| limit as usize)
                    .unwrap_or(DEFAULT_SYSTEM_MEMORY_SIDECAR_LIMIT)
                    .min(12);
                for m in memories.iter().take(limit) {
                    combined.push_str(&system_memory::format_for_signpost(m));
                    combined.push('\n');
                }
                if memories.len() > limit {
                    combined.push_str(&format!(
                        "  [truncated system memories: showing {limit} of {}; query category=\"system_memory\" or an exact sm-* id for more]\n",
                        memories.len()
                    ));
                }
                combined.push_str(
                    "  (signposts only; query an exact sm-* id for bounded full-body pages)\n",
                );
            }

            // Top-level breadcrumb: pull the highest-ranked knowledge entry into
            // the graph funnel. Packets and memories carry their own pointers
            // above; this completes the response-breadcrumb plane for entries.
            if let Some(id) = &top_entry_id {
                let entity_ref = knowledge_entity_ref(id);
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Next steps ───────────────────────────────\n");
                combined.push_str(&format!(
                    "  → Inspect the top entry's edges + provenance: bbox_inspect_entity(entity_ref=\"{entity_ref}\")\n"
                ));
                combined.push_str(&format!(
                    "  → Package an answer: bbox_bundle_evidence(question=<q>, entity_refs=[\"{entity_ref}\"])\n"
                ));
            }
            let mut structured = view.structured_response(&returned_ids);
            structured["diagnostics"] = diagnostic_summary(&view.diagnostics, &p);
            bound_structured_knowledge_rows(&mut structured, &p);
            let diagnostic_text = diagnostic_summary_text(&structured["diagnostics"]);
            if !diagnostic_text.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&diagnostic_text);
                combined.push('\n');
            }
            combined = view.append_built_from_for_ids(combined, &returned_ids);
            Ok((combined, structured))
        })
        .await
    }

    #[tool(name = "bbox_knowledge_link", description = "Append a knowledge edge.")]
    pub(crate) async fn bbox_knowledge_link(
        &self,
        Parameters(p): Parameters<KnowledgeLinkParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            if let Some(text) = server.enqueue_link_via_checkout_owner(&p)? {
                return Ok::<_, anyhow::Error>((text, KnowledgeMutationOwner::CheckoutQueue));
            }
            let target = server.prepare_existing_knowledge_mutation(&p.source)?;
            p.source = format!("knowledge:{}", target.id);
            let edge = server.state.kb.write().append_link_with_write_dir(
                &p,
                target
                    .carrier
                    .as_ref()
                    .map(|carrier| carrier.carrier_id.as_str()),
                target.seed.as_ref(),
            )?;
            server.finish_existing_knowledge_mutation(target.checkout.as_ref());
            Ok::<_, anyhow::Error>((
                serde_json::to_string_pretty(&json!({
                    "status": "linked",
                    "source": p.source,
                    "target": p.target,
                    "kind": edge.kind.edge_kind(),
                    "confidence": edge.confidence,
                }))?,
                KnowledgeMutationOwner::Local,
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge link task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((text, owner)) => {
                if let Err(e) = self.persist_knowledge_mutation(owner).await {
                    log_tool_err("bbox_knowledge_link", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                log_tool_ok("bbox_knowledge_link", start, text.len());
                Self::ok_text(&text)
            }
            Err(e) => {
                log_tool_err("bbox_knowledge_link", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(name = "bbox_forget", description = "Retire or supersede an entry.")]
    pub(crate) async fn bbox_forget(
        &self,
        Parameters(p): Parameters<ForgetParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            if let Some(message) = server.enqueue_forget_via_checkout_owner(&p)? {
                return Ok::<_, anyhow::Error>((
                    message,
                    p.id.clone(),
                    true,
                    KnowledgeMutationOwner::CheckoutQueue,
                ));
            }
            let target = server.prepare_existing_knowledge_mutation(&p.id)?;
            p.id = target.id.clone();
            let message = server.state.kb.write().forget_with_write_dir(
                &p,
                target
                    .carrier
                    .as_ref()
                    .map(|carrier| carrier.carrier_id.as_str()),
                target.seed.as_ref(),
            )?;
            server.finish_existing_knowledge_mutation(target.checkout.as_ref());
            Ok::<_, anyhow::Error>((
                message,
                target.id,
                target.checkout.is_some(),
                KnowledgeMutationOwner::Local,
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge forget task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((message, id, provisional, owner)) => {
                if let Err(e) = self.persist_knowledge_mutation(owner).await {
                    log_tool_err("bbox_forget", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if !provisional && let Err(err) = self.tombstone_knowledge_entry_in_index(&id) {
                    tracing::warn!(error = %err, entry = %id, "knowledge index tombstone failed; will reconstruct on next reindex cycle");
                }
                log_tool_ok("bbox_forget", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_forget", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "knowledge_queue_tests.rs"]
mod queue_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn bind_remote_workspace(server: &BlackboxServer) {
        assert!(
            server
                .session_workspace_binding
                .set(Some(std::sync::Arc::new(
                    crate::server::knowledge_source::WorkspaceBindingGrant {
                        task_id: "task-bound".into(),
                        session_id: "session-bound".into(),
                        project_id: "p_bound".into(),
                        scope: bbox_corpus_core::identity::PublishedScope::try_new(
                            "bound-test",
                            "."
                        )
                        .unwrap(),
                        workspace_id: bro_core::WorkspaceId::parse(
                            "0123456789abcdef0123456789abcdef"
                        )
                        .unwrap(),
                        expires_unix_secs: u64::MAX,
                    },
                )))
                .is_ok()
        );
    }

    #[test]
    fn bound_workspace_daemon_lane_accepts_global_and_refuses_project_knowledge() {
        let directory = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(directory.path()),
        ));
        bind_remote_workspace(&server);

        assert!(
            server
                .guard_workspace_bound_project_knowledge(Some("global"))
                .is_ok()
        );
        let error = server
            .guard_workspace_bound_project_knowledge(Some("project"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("knowledge_transport_authoritative")
        );
    }

    fn init_system_memory() {
        crate::init_system_memory_for_tests();
    }

    #[test]
    fn first_entry_id_extracts_top_and_handles_sentinel() {
        let block = "2 entries:\n\n[abc123] Convention/project | all | title\n  \
                     content_bytes=10\n  body [with brackets]\n\n[def456] ...";
        assert_eq!(first_entry_id(block).as_deref(), Some("abc123"));
        assert_eq!(first_entry_id("No entries found."), None);
        assert_eq!(first_entry_id(""), None);
    }

    #[test]
    fn overlay_access_classification_matches_reconciliation_staleness() {
        use bbox_indexing::checkout_access::{CheckoutAccessError, CheckoutAccessErrorCode};
        use bbox_knowledge::overlay::OverlayRecomputeErrorKind;

        let stale =
            classify_knowledge_overlay_access_error(anyhow::Error::new(CheckoutAccessError::new(
                CheckoutAccessErrorCode::AttachmentInactive,
                "inactive test attachment",
            )));
        assert_eq!(stale.kind, OverlayRecomputeErrorKind::InvalidContent);

        let transient =
            classify_knowledge_overlay_access_error(anyhow::Error::new(CheckoutAccessError::new(
                CheckoutAccessErrorCode::ObservationUnavailable,
                "temporary observation failure",
            )));
        assert_eq!(transient.kind, OverlayRecomputeErrorKind::Transient);
    }

    #[test]
    fn exact_system_memory_response_returns_only_exact_memory() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            query: Some("sm-refactor".into()),
            ..Default::default()
        })
        .expect("exact canonical system memory query should short-circuit");

        assert!(out.contains("[system] sm-refactor"));
        assert!(!out.contains("[system] sm-refactor-rust"));
        assert!(!out.contains("[bb-tool-reference]"));
        assert!(!out.contains("No entries found."));
    }

    #[test]
    fn exact_system_memory_response_respects_runtime_filters() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            scope: Some("project".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });

        assert!(out.is_none());
    }

    #[test]
    fn system_memory_catalog_returns_all_memories() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("system_memory".into()),
            ..Default::default()
        })
        .expect("system_memory category should return catalog");

        assert!(out.contains("── System memories"));
        assert!(out.contains("[system] sm-rule-packets"));
        assert!(out.contains("[system] sm-refactor"));
        assert!(out.contains("[system] sm-agentic-opening-sequence"));
        assert!(
            !out.contains("bbox_compile"),
            "catalog listing should not include full body"
        );
    }

    #[test]
    fn system_memory_catalog_accepts_hyphenated_and_plural_forms() {
        init_system_memory();
        for form in &["system_memory", "system-memory", "system_memories"] {
            let out = exact_system_memory_response(&KnowledgeListParams {
                category: Some(form.to_string()),
                ..Default::default()
            });
            assert!(out.is_some(), "category={} should match", form);
        }
    }

    #[test]
    fn system_memory_catalog_supports_query_filter() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("system_memory".into()),
            query: Some("refactor".into()),
            ..Default::default()
        })
        .expect("system_memory + query should return filtered catalog");

        assert!(out.contains("[system] sm-refactor"));
        assert!(out.contains("[system] sm-refactor-rust"));
        assert!(!out.contains("[system] sm-rule-packets"));
    }

    #[test]
    fn system_memory_category_does_not_match_memory() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("memory".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });
        assert!(out.is_none());
    }

    fn init_repo_with_worktree(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::process::Command;
        let base = tmp.join("repo");
        std::fs::create_dir_all(&base).unwrap();
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Blackbox Test",
                "-c",
                "user.email=blackbox@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(&base)
                .args(&args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let worktree = tmp.join("wt");
        let out = Command::new("git")
            .arg("-C")
            .arg(&base)
            .args([
                "worktree",
                "add",
                "-b",
                "arc/scoped",
                worktree.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (
            base.canonicalize().unwrap(),
            worktree.canonicalize().unwrap(),
        )
    }

    /// Test server whose bridge registry carries the base repo, so the
    /// engine-backed rescoper resolves against real registered records.
    fn server_with_registered(
        state_root: &std::path::Path,
        base: &std::path::Path,
    ) -> (
        crate::server::BlackboxServer,
        crate::projects::ProjectRecord,
    ) {
        let server = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(state_root),
        ));
        let record = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(base)
            .unwrap();
        (server, record)
    }

    #[test]
    fn rescope_project_filter_resolves_worktree_to_base_with_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, worktree) = init_repo_with_worktree(&tmp_root);
        let (server, _record) = server_with_registered(&tmp_root, &base);

        let mut p = KnowledgeListParams {
            project: Some(worktree.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_project_filter(&server, &mut p);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_alias.as_deref(), Some(worktree.to_str().unwrap()));

        // A plain descendant collapses to the base with no alias needed
        // (descendant entry paths contain the base path already).
        let subdir = base.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let mut p = KnowledgeListParams {
            project: Some(subdir.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_project_filter(&server, &mut p);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_alias, None);
    }

    #[test]
    fn rescope_project_filter_leaves_non_path_and_unregistered_filters_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, _record) = server_with_registered(&tmp_root, &base);

        // Substring filter (not an absolute path) is untouched, and the
        // caller is told the value resolved to nothing (gap-40ab1102).
        let mut p = KnowledgeListParams {
            project: Some("transcript-search".into()),
            ..Default::default()
        };
        let diagnostic = rescope_project_filter(&server, &mut p)
            .expect("an unresolvable filter must report itself");
        assert!(diagnostic.contains("transcript-search"), "{diagnostic}");
        assert_eq!(p.project.as_deref(), Some("transcript-search"));
        assert_eq!(p.project_alias, None);
        assert_eq!(p.project_id, None);

        // An absolute path no registered project owns is untouched.
        let stranger = tmp_root.join("stranger");
        std::fs::create_dir_all(&stranger).unwrap();
        let mut p = KnowledgeListParams {
            project: Some(stranger.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let diagnostic = rescope_project_filter(&server, &mut p)
            .expect("an unresolvable filter must report itself");
        assert!(
            diagnostic.contains(stranger.to_str().unwrap()),
            "{diagnostic}"
        );
        assert_eq!(p.project.as_deref(), Some(stranger.to_str().unwrap()));
        assert_eq!(p.project_alias, None);
    }

    #[test]
    fn rescope_project_filter_accepts_alias_and_id_selectors() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, record) = server_with_registered(&tmp_root, &base);
        // Declared aliases materialize through the same registry sync the
        // register adapter runs.
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .sync_declared_aliases(
                &record.project_id,
                &["blackbox".to_string()].into_iter().collect(),
            )
            .unwrap();

        // A registered alias rewrites to the base canonical path AND arms
        // the id predicate, so rows carrying only a project_id match too
        // (gap-40ab1102).
        let mut p = KnowledgeListParams {
            project: Some("blackbox".into()),
            ..Default::default()
        };
        assert_eq!(rescope_project_filter(&server, &mut p), None);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_alias, None);
        assert_eq!(p.project_id.as_deref(), Some(record.project_id.as_str()));

        // A project_id selector rewrites the same way.
        let mut p = KnowledgeListParams {
            project: Some(record.project_id.clone()),
            ..Default::default()
        };
        assert_eq!(rescope_project_filter(&server, &mut p), None);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_id.as_deref(), Some(record.project_id.as_str()));

        // A caller-supplied id is respected rather than overwritten.
        let mut p = KnowledgeListParams {
            project: Some(record.project_id.clone()),
            project_id: Some("caller-supplied".into()),
            ..Default::default()
        };
        assert_eq!(rescope_project_filter(&server, &mut p), None);
        assert_eq!(p.project_id.as_deref(), Some("caller-supplied"));
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn path_cut_rejects_unregistered_project_write_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("unregistered");
        std::fs::create_dir_all(&project).unwrap();
        let server = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(tmp.path()),
        ));
        server
            .state
            .path_fallback_cut
            .store(true, std::sync::atomic::Ordering::Release);
        let mut project = Some(project.to_string_lossy().into_owned());
        let err = server
            .prepare_knowledge_write(&mut project, &mut None)
            .unwrap_err();
        assert!(err.to_string().contains("project writes require"));
    }

    /// End-to-end repro of gap-de82a74d: an agent inside an in-tree linked
    /// worktree (`<root>/.claude/worktrees/<name>`) learns a project-scoped
    /// entry. The entry must key to the registered BASE (durable scope), the
    /// committed `.bbox/knowledge/` file must land in the WORKTREE (travels
    /// with the branch, never mutates the base checkout), and an immediate
    /// `bbox_render` from the same worktree must include the entry — the
    /// asymmetry that motivated the gap.
    #[tokio::test]
    async fn bbox_learn_from_worktree_keys_base_writes_worktree_and_renders() {
        use crate::knowledge::RenderParams;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        // Repo-owned: the checkout (and thus the worktree) carries .bbox/knowledge/.
        std::fs::create_dir_all(base.join(".bbox").join("knowledge")).unwrap();
        let repo_id = crate::config::ensure_recorded_repo_id(&base)
            .unwrap()
            .repo_id;
        std::fs::write(base.join(".bbox").join("knowledge").join(".gitkeep"), "").unwrap();
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/kb",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = worktree.canonicalize().unwrap();
        let wt = wt_canon.to_string_lossy().into_owned();

        let server = crate::server::BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(tmp.path()),
        ));
        let project_id = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap()
            .project_id;

        let learn = server
            .bbox_learn(Parameters(LearnParams {
                content: "WORKTREE_KB_MARKER: prefer rustls".into(),
                category: "convention".into(),
                scope: Some("project".into()),
                project: Some(wt.clone()),
                ..Default::default()
            }))
            .await;
        assert_ne!(learn.is_error, Some(true), "learn failed: {learn:?}");

        // Durable scope = registered base; committed file = worktree checkout.
        // Provisional bytes are intentionally absent from the central store.
        let ids = std::fs::read_dir(wt_canon.join(".bbox/knowledge"))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|ext| ext.to_str()) == Some("json"))
                    .then(|| path.file_stem().unwrap().to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 1, "{ids:?}");
        let id = ids[0].clone();
        assert!(server.state.kb.read().entry(&id).is_none());
        let rel = std::path::Path::new(".bbox")
            .join("knowledge")
            .join(format!("{id}.json"));
        assert!(
            wt_canon.join(&rel).exists(),
            "committed entry file must land in the worktree"
        );
        assert!(
            !base_canon.join(&rel).exists(),
            "the daemon must not mutate the base checkout"
        );

        // Slice 3.3 dark state: the composite registry row was durable before
        // the write, and the post-write overlay sees the new untracked entry
        // without affecting the still-legacy retrieval behavior asserted above.
        let scope = bbox_corpus_core::identity::PublishedScope::try_new(repo_id, ".").unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&wt_canon).unwrap();
        assert!(
            server
                .state
                .checkout_registry
                .read()
                .get(&checkout_id, &scope)
                .is_some()
        );
        let overlays = server.state.knowledge_overlays.read();
        let snapshot = overlays.get(&scope, &checkout_id).expect("dark overlay");
        assert_eq!(
            snapshot.status,
            bbox_knowledge::overlay::OverlayStatus::Valid,
            "{snapshot:?}"
        );
        assert!(matches!(
            snapshot.values.get(&id),
            Some(bbox_knowledge::overlay::OverlayValue::Upsert { .. })
        ));
        drop(overlays);

        // Tool arguments cannot grant own-checkout visibility. Model the MCP
        // transport authority that a real worktree session records at init.
        server.set_session_checkout_for_test(project_id, scope, checkout_id, wt_canon.clone());
        let own = server
            .session_knowledge_view(Some(base_canon.to_str().unwrap()), Some("own"))
            .unwrap();
        let own_entry = own
            .items
            .iter()
            .find(|item| item.entry.id == id)
            .expect("checkout entry visible through own overlay");
        assert_eq!(
            own_entry.entry.project.as_deref(),
            Some(base_canon.to_string_lossy().as_ref()),
            "overlay entry must key to the registered base, not the worktree"
        );

        // The other half of the gap: render from the worktree sees the entry.
        let render = server
            .bbox_render(Parameters(RenderParams {
                provider: Some("claude".into()),
                project: Some(wt.clone()),
                scope: Some("project".into()),
                dry_run: Some(false),
                ..Default::default()
            }))
            .await;
        assert_ne!(render.is_error, Some(true), "render failed: {render:?}");
        let rendered = std::fs::read_to_string(wt_canon.join("CLAUDE.md")).unwrap();
        assert!(
            rendered.contains("WORKTREE_KB_MARKER"),
            "worktree render must include the just-learned entry: {rendered}"
        );

        // A checkout write whose identity cannot be reconstructed after a
        // restart fails closed before writing another provisional file.
        let local = wt_canon.join(".bbox/local");
        std::fs::remove_dir_all(&local).unwrap();
        std::fs::write(&local, "block overlay marker creation").unwrap();
        let degraded = server
            .bbox_learn(Parameters(LearnParams {
                content: "WORKTREE_KB_DEGRADED_MARKER: legacy write survives".into(),
                category: "convention".into(),
                scope: Some("project".into()),
                project: Some(wt),
                ..Default::default()
            }))
            .await;
        assert_eq!(degraded.is_error, Some(true), "{degraded:?}");
        let remaining = std::fs::read_dir(wt_canon.join(".bbox/knowledge"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .count();
        assert_eq!(remaining, 1, "failed admission must not write a new entry");
    }

    /// A project-scoped entry stamped with project identity and NO path key,
    /// the shape every catalog-published row has.
    fn stamped_entry(
        id: &str,
        content: &str,
        project_id: &str,
    ) -> crate::knowledge::KnowledgeEntry {
        use bbox_knowledge::knowledge::{Approval, Category, Priority, Scope, Status};
        crate::knowledge::KnowledgeEntry {
            id: id.into(),
            title: id.into(),
            content: content.into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: Some(project_id.to_string()),
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

    fn knowledge_rows(structured: &serde_json::Value) -> &Vec<serde_json::Value> {
        structured["rows"].as_array().expect("rows array")
    }

    fn knowledge_diagnostics(structured: &serde_json::Value) -> Vec<String> {
        structured["diagnostics"]["previews"]
            .as_array()
            .expect("diagnostic preview array")
            .iter()
            .map(|line| {
                line.as_str()
                    .expect("diagnostic preview string")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn diagnostic_summary_keeps_many_warnings_compact_and_recoverable() {
        let p = KnowledgeListParams {
            query: Some("no such marker".into()),
            project: Some("global-lane-selector".into()),
            ..Default::default()
        };
        let diagnostics: Vec<String> = (0..24)
            .map(|index| match index % 4 {
                0 => format!("catalog index unavailable for shard {index}"),
                1 => format!("accepted publication is stale for shard {index}"),
                2 => format!("checkout mutation queued for shard {index}"),
                _ => format!("overlay partially loaded for shard {index}"),
            })
            .collect();
        let summary = diagnostic_summary(&diagnostics, &p);
        assert_eq!(summary["count"], 24);
        assert_eq!(summary["states"]["unavailable"], 6);
        assert_eq!(summary["states"]["stale"], 6);
        assert_eq!(summary["states"]["queued"], 6);
        assert_eq!(summary["states"]["partial"], 6);
        assert_eq!(summary["previews"].as_array().unwrap().len(), 2);
        assert_eq!(
            summary["recovery"]["arguments"]["project"],
            "global-lane-selector"
        );
        assert_eq!(summary["recovery"]["arguments"]["query"], "no such marker");
        assert_eq!(summary["recovery"]["arguments"]["diagnostics_detail"], true);
        let text = diagnostic_summary_text(&summary);
        assert!(text.contains("24 diagnostics"));
        assert!(text.contains("unavailable=6"));
        assert!(!text.contains("shard 23"));
    }

    #[test]
    fn exact_diagnostics_pages_preserve_scope_and_reconstruct_unicode() {
        let p = KnowledgeListParams {
            query: Some("unicode diagnostics".into()),
            project: Some("unregistered-global-lane".into()),
            detail_limit: Some(257),
            ..Default::default()
        };
        let diagnostics: Vec<String> = (0..12)
            .map(|index| format!("diagnostic {index}: {}", "警告".repeat(80)))
            .collect();
        let expected = serde_json::to_string(&diagnostics).unwrap();
        let (_, first) = exact_diagnostics_response(&diagnostics, &p).unwrap();
        assert_eq!(first["scope"]["project"], "unregistered-global-lane");
        assert_eq!(first["scope"]["query"], "unicode diagnostics");

        let mut cursor = first["body"]["next_cursor"].as_str().map(str::to_string);
        let mut reconstructed = first["body"]["text"].as_str().unwrap().to_string();
        let mut calls = 1;
        while let Some(active_cursor) = cursor {
            let mut next_params = p.clone();
            next_params.detail_cursor = Some(active_cursor.clone());
            let (_, page) = exact_diagnostics_response(&diagnostics, &next_params).unwrap();
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
            calls += 1;
        }
        assert!(calls > 1);
        assert_eq!(reconstructed, expected);

        let mut stale_params = p.clone();
        stale_params.detail_cursor = first["body"]["next_cursor"].as_str().map(str::to_string);
        let mut changed = diagnostics;
        changed[0].push_str(" changed");
        let error = exact_diagnostics_response(&changed, &stale_params)
            .expect_err("changed diagnostics must invalidate cursor");
        assert!(error.to_string().contains("stale detail cursor"));
    }

    #[test]
    fn projection_body_pages_reconstruct_unicode_and_reject_changed_content() {
        let body = "境界".repeat(1_000);
        let first = projection_body_page("test body", "test scope", &body, "text", None, Some(257))
            .unwrap();
        assert_eq!(first["total_bytes"], body.len());
        let mut reconstructed = first["text"].as_str().unwrap().to_string();
        let mut cursor = first["next_cursor"].as_str().map(str::to_string);
        while let Some(active_cursor) = cursor {
            let page = projection_body_page(
                "test body",
                "test scope",
                &body,
                "text",
                Some(&active_cursor),
                Some(257),
            )
            .unwrap();
            reconstructed.push_str(page["text"].as_str().unwrap());
            cursor = page["next_cursor"].as_str().map(str::to_string);
        }
        assert_eq!(reconstructed, body);
        let stale = projection_body_page(
            "test body",
            "test scope",
            "changed",
            "text",
            first["next_cursor"].as_str(),
            Some(257),
        )
        .expect_err("changed body must invalidate cursor");
        assert!(stale.to_string().contains("stale detail cursor"));
    }

    #[test]
    fn exact_system_memory_read_pages_oversized_record() {
        init_system_memory();
        let memory =
            system_memory::exact_query(Some("sm-rule-packets")).expect("canonical system memory");
        assert!(memory.content.len() > KNOWLEDGE_DETAIL_PAGE_BYTES);
        let p = KnowledgeListParams {
            query: Some(memory.id.clone()),
            detail_limit: Some(257),
            ..Default::default()
        };
        let (text, first) = exact_system_memory_detail_response(memory, &p).unwrap();
        assert!(text.contains("structuredContent.body"));
        assert!(first["body"]["text"].as_str().unwrap().len() <= 257);
        let mut reconstructed = first["body"]["text"].as_str().unwrap().to_string();
        let mut cursor = first["body"]["next_cursor"].as_str().map(str::to_string);
        while let Some(active_cursor) = cursor {
            let mut next = p.clone();
            next.detail_cursor = Some(active_cursor);
            let (_, page) = exact_system_memory_detail_response(memory, &next).unwrap();
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
        }
        let recovered: serde_json::Value = serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(recovered["id"], memory.id);
        assert_eq!(recovered["title"], memory.title);
        assert_eq!(recovered["tags"], json!(memory.tags));
        assert_eq!(recovered["content"], memory.content);
    }

    #[test]
    fn structured_knowledge_rows_replace_oversized_content_with_detail() {
        let content = "知".repeat(2_000);
        let mut structured = json!({
            "rows": [{
                "entity_ref": "knowledge:big00001",
                "entry": {"id": "big00001", "content": content},
            }]
        });
        let p = KnowledgeListParams {
            project: Some("/registered/project".into()),
            query: Some("escaped \"metadata\"".into()),
            provisional: Some("all".into()),
            ..Default::default()
        };
        bound_structured_knowledge_rows(&mut structured, &p);
        let entry = &structured["rows"][0]["entry"];
        assert!(entry["content"].as_str().unwrap().len() <= STRUCTURED_KNOWLEDGE_CONTENT_BYTES);
        assert_eq!(entry["content_bytes"], content.len());
        assert_eq!(entry["content_truncated"], true);
        let arguments = &structured["rows"][0]["detail"]["arguments"];
        assert_eq!(arguments["entry_detail"], "knowledge:big00001");
        assert_eq!(arguments["project"], "/registered/project");
        assert_eq!(arguments["query"], "escaped \"metadata\"");
        assert_eq!(arguments["provisional"], "all");
    }

    #[test]
    fn structured_knowledge_rows_bound_oversized_metadata_and_collections() {
        let title = "\"metadata\"\t".repeat(256);
        let rationale = "decision ".repeat(128);
        let mut structured = json!({
            "rows": [{
                "entity_ref": "provisional_knowledge:project:checkout:metadata",
                "entry": {
                    "id": "metadata",
                    "title": title,
                    "content": "compact",
                    "rationale": rationale,
                    "variants": {"provider": "\"variant\" ".repeat(128)},
                    "providers": ["a", "b"],
                },
            }]
        });
        let p = KnowledgeListParams {
            category: Some("convention".into()),
            ..Default::default()
        };
        bound_structured_knowledge_rows(&mut structured, &p);
        let entry = &structured["rows"][0]["entry"];
        assert!(entry["title"].as_str().unwrap().len() <= STRUCTURED_KNOWLEDGE_METADATA_BYTES + 32);
        assert_eq!(entry["title_bytes"], title.len());
        assert!(
            entry["rationale"].as_str().unwrap().len() <= STRUCTURED_KNOWLEDGE_METADATA_BYTES + 32
        );
        assert_eq!(entry["rationale_bytes"], rationale.len());
        assert_eq!(entry["variants"]["count"], 1);
        assert_eq!(entry["variants"]["truncated"], true);
        let arguments = &structured["rows"][0]["detail"]["arguments"];
        assert_eq!(
            arguments["entry_detail"],
            "provisional_knowledge:project:checkout:metadata"
        );
        assert_eq!(arguments["category"], "convention");
    }

    #[test]
    fn exact_entry_detail_binds_canonical_provisional_variant_and_scope() {
        use crate::server::knowledge_view::{KnowledgeViewItem, SessionKnowledgeView};

        let own_ref = "provisional_knowledge:project:own-checkout:shared".to_string();
        let peer_ref = "provisional_knowledge:project:peer-checkout:shared".to_string();
        let published_ref = "knowledge:shared".to_string();
        let mut published = stamped_entry("shared", "published variant", "project");
        let mut own = stamped_entry("shared", "own variant", "project");
        own.content = "own ".repeat(256);
        let mut peer = stamped_entry("shared", "peer variant", "project");
        peer.content = "peer ".repeat(256);

        let mut store_entries = Vec::new();
        let mut items = Vec::new();
        for (entity_ref, entry) in [
            (published_ref.clone(), published.clone()),
            (own_ref.clone(), own.clone()),
            (peer_ref.clone(), peer.clone()),
        ] {
            let item_entry = entry.clone();
            store_entries.push(entry);
            items.push(KnowledgeViewItem {
                entity_ref,
                entry: item_entry,
                metadata: Default::default(),
            });
        }
        let view = SessionKnowledgeView {
            knowledge: bbox_knowledge::knowledge::Knowledge::detached_view(
                store_entries,
                BTreeMap::new(),
            ),
            items,
            built_from: Default::default(),
            diagnostics: Vec::new(),
            degraded_overlays: Vec::new(),
        };
        let visible_refs = [published_ref, own_ref.clone(), peer_ref.clone()];
        let p = KnowledgeListParams {
            entry_detail: Some(own_ref.clone()),
            provisional: Some("all".into()),
            detail_limit: Some(257),
            ..Default::default()
        };

        let (_, first) = exact_entry_detail_response(&view, &p, &visible_refs).unwrap();
        assert_eq!(first["entity_ref"], own_ref);
        assert_eq!(first["scope"]["provisional"], "all");
        let cursor = first["body"]["next_cursor"].as_str().unwrap().to_string();
        let mut changed_scope = p.clone();
        changed_scope.query = Some("changed scope".into());
        changed_scope.detail_cursor = Some(cursor);
        let stale = exact_entry_detail_response(&view, &changed_scope, &visible_refs)
            .expect_err("changed filter scope must invalidate cursor");
        assert!(stale.to_string().contains("stale detail cursor"));

        let mut bare = p.clone();
        bare.entry_detail = Some("shared".into());
        let ambiguous = exact_entry_detail_response(&view, &bare, &visible_refs)
            .expect_err("duplicate bare id must reject");
        assert!(ambiguous.to_string().contains("ambiguous across variants"));
        assert!(ambiguous.to_string().contains(&own_ref));

        let mut hidden = p.clone();
        hidden.entry_detail = Some(peer_ref);
        let out_of_scope = exact_entry_detail_response(&view, &hidden, &[own_ref])
            .expect_err("filter scope must reject a hidden variant");
        assert!(
            out_of_scope
                .to_string()
                .contains("not in the requested filter scope")
        );
    }

    #[tokio::test]
    async fn bbox_knowledge_exact_entry_pages_bound_and_reconstruct_unicode() {
        init_system_memory();
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, record) = server_with_registered(&tmp_root, &base);
        let content = "體".repeat(4_000);
        server
            .state
            .kb
            .write()
            .upsert_generated(stamped_entry("unicode-entry", &content, &record.project_id))
            .unwrap();

        let default = server
            .bbox_knowledge(Parameters(KnowledgeListParams {
                query: Some(content[..6].into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(default.is_error, Some(true), "{default:?}");
        let default = default.structured_content.expect("structured rows");
        let row = &default["rows"][0];
        assert!(
            row["entry"]["content"].as_str().unwrap().len() <= STRUCTURED_KNOWLEDGE_CONTENT_BYTES
        );
        assert_eq!(row["entry"]["content_bytes"], content.len());
        assert_eq!(
            row["detail"]["arguments"]["entry_detail"],
            "knowledge:unicode-entry"
        );

        let first = server
            .bbox_knowledge(Parameters(KnowledgeListParams {
                entry_detail: Some("knowledge:unicode-entry".into()),
                detail_limit: Some(257),
                ..Default::default()
            }))
            .await;
        assert_ne!(first.is_error, Some(true), "{first:?}");
        let first = first.structured_content.expect("structured detail");
        assert_eq!(first["entity_ref"], "knowledge:unicode-entry");
        let mut reconstructed = first["body"]["text"].as_str().unwrap().to_string();
        let mut cursor = first["body"]["next_cursor"].as_str().map(str::to_string);
        while let Some(active_cursor) = cursor {
            let page = server
                .bbox_knowledge(Parameters(KnowledgeListParams {
                    entry_detail: Some("knowledge:unicode-entry".into()),
                    detail_cursor: Some(active_cursor.clone()),
                    detail_limit: Some(257),
                    ..Default::default()
                }))
                .await;
            assert_ne!(page.is_error, Some(true), "{page:?}");
            let page = page.structured_content.expect("structured detail page");
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
        }
        let recovered: crate::knowledge::KnowledgeEntry =
            serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(recovered.content, content);

        server
            .state
            .kb
            .write()
            .upsert_generated(stamped_entry(
                "unicode-entry",
                "changed body",
                &record.project_id,
            ))
            .unwrap();
        let stale = server
            .bbox_knowledge(Parameters(KnowledgeListParams {
                entry_detail: Some("knowledge:unicode-entry".into()),
                detail_cursor: first["body"]["next_cursor"].as_str().map(str::to_string),
                detail_limit: Some(257),
                ..Default::default()
            }))
            .await;
        assert_eq!(stale.is_error, Some(true), "{stale:?}");
        assert!(format!("{stale:?}").contains("stale detail cursor"));
    }

    #[tokio::test]
    async fn bbox_knowledge_exact_metadata_pages_stay_inside_serialized_envelope() {
        init_system_memory();
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, record) = server_with_registered(&tmp_root, &base);
        let mut entry = stamped_entry("metadata-entry", "small content", &record.project_id);
        entry.title = "\"metadata title\"\t".repeat(2_000);
        entry.rationale = Some("\"rationale\"\n".repeat(2_000));
        server
            .state
            .kb
            .write()
            .upsert_generated(entry.clone())
            .unwrap();

        let mut params = KnowledgeListParams {
            entry_detail: Some("knowledge:metadata-entry".into()),
            detail_limit: Some(257),
            ..Default::default()
        };
        let first = server.bbox_knowledge(Parameters(params.clone())).await;
        assert_ne!(first.is_error, Some(true), "{first:?}");
        let wire = serde_json::to_vec(&first).unwrap();
        assert!(
            wire.len() <= BlackboxServer::MCP_RESPONSE_CAP_BYTES,
            "serialized envelope was {} bytes",
            wire.len()
        );
        let first = first.structured_content.expect("structured detail");
        assert_eq!(first["entity_ref"], "knowledge:metadata-entry");
        let mut reconstructed = first["body"]["text"].as_str().unwrap().to_string();
        let mut cursor = first["body"]["next_cursor"].as_str().map(str::to_string);
        while let Some(active_cursor) = cursor {
            params.detail_cursor = Some(active_cursor);
            let page = server.bbox_knowledge(Parameters(params.clone())).await;
            assert_ne!(page.is_error, Some(true), "{page:?}");
            let wire = serde_json::to_vec(&page).unwrap();
            assert!(wire.len() <= BlackboxServer::MCP_RESPONSE_CAP_BYTES);
            let page = page.structured_content.expect("structured detail page");
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
        }
        let recovered: crate::knowledge::KnowledgeEntry =
            serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(recovered.title, entry.title);
        assert_eq!(recovered.rationale, entry.rationale);
    }

    /// gap-40ab1102 (1): a `project` filter must match rows by their stamped
    /// project id. A row that carries a project_id and no path key (every
    /// catalog-published row) was dropped by all three selectors before the
    /// filter armed the id predicate, so a project with knowledge answered
    /// every filtered query with nothing.
    #[tokio::test]
    async fn bbox_knowledge_project_filter_matches_stamped_rows_by_id_alias_and_path() {
        init_system_memory();
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, record) = server_with_registered(&tmp_root, &base);
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .sync_declared_aliases(
                &record.project_id,
                &["kb-filter-alias".to_string()].into_iter().collect(),
            )
            .unwrap();
        server
            .state
            .kb
            .write()
            .upsert_generated(stamped_entry(
                "stamped-row",
                "STAMPED_ROW_MARKER",
                &record.project_id,
            ))
            .unwrap();

        for selector in [
            record.project_id.as_str(),
            "kb-filter-alias",
            base.to_str().unwrap(),
        ] {
            let result = server
                .bbox_knowledge(Parameters(KnowledgeListParams {
                    project: Some(selector.to_string()),
                    ..Default::default()
                }))
                .await;
            assert_ne!(result.is_error, Some(true), "{selector}: {result:?}");
            let structured = result
                .structured_content
                .expect("bbox_knowledge structured response");
            let rows = knowledge_rows(&structured);
            assert_eq!(rows.len(), 1, "selector {selector}: {structured}");
            assert_eq!(rows[0]["entry"]["id"], "stamped-row", "{selector}");
        }
    }

    /// gap-40ab1102 (1): a filter value that names no registered project
    /// keeps literal substring semantics AND reports itself, so an empty
    /// result cannot be read as an empty store.
    #[tokio::test]
    async fn bbox_knowledge_unresolvable_project_filter_reports_the_value() {
        init_system_memory();
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, record) = server_with_registered(&tmp_root, &base);
        server
            .state
            .kb
            .write()
            .upsert_generated(stamped_entry(
                "stamped-row",
                "STAMPED_ROW_MARKER",
                &record.project_id,
            ))
            .unwrap();

        let result = server
            .bbox_knowledge(Parameters(KnowledgeListParams {
                project: Some("no-such-project-selector".into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let structured = result
            .structured_content
            .expect("bbox_knowledge structured response");
        assert!(knowledge_rows(&structured).is_empty(), "{structured}");
        let diagnostics = knowledge_diagnostics(&structured).join("\n");
        assert!(
            diagnostics.contains("no-such-project-selector"),
            "the diagnostic preview must name the unresolvable value: {diagnostics}"
        );
        assert!(
            diagnostics.contains("resolved to no registered project"),
            "{diagnostics}"
        );
        assert_eq!(structured["diagnostics"]["count"], 1);
        assert_eq!(structured["diagnostics"]["states"]["filter"], 1);
        assert_eq!(
            structured["diagnostics"]["recovery"]["arguments"]["diagnostics_detail"],
            true
        );
    }

    /// gap-40ab1102 (3): the legacy-lane diagnostic describes ROWS, so a
    /// response that returned none of them must not carry it. Firing it on
    /// every response, stamped or not, is what trains callers to ignore
    /// diagnostics.
    #[tokio::test]
    async fn bbox_knowledge_legacy_diagnostic_only_rides_responses_with_legacy_rows() {
        init_system_memory();
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, _worktree) = init_repo_with_worktree(&tmp_root);
        let (server, record) = server_with_registered(&tmp_root, &base);
        server
            .state
            .kb
            .write()
            .upsert_generated(stamped_entry(
                "stamped-row",
                "STAMPED_ROW_MARKER",
                &record.project_id,
            ))
            .unwrap();

        let matching = server
            .bbox_knowledge(Parameters(KnowledgeListParams {
                query: Some("STAMPED_ROW_MARKER".into()),
                ..Default::default()
            }))
            .await
            .structured_content
            .expect("bbox_knowledge structured response");
        assert_eq!(knowledge_rows(&matching).len(), 1, "{matching}");
        assert!(
            knowledge_diagnostics(&matching)
                .iter()
                .any(|diagnostic| diagnostic.contains("legacy_compatibility")),
            "a returned legacy row keeps the warning: {matching}"
        );

        let empty = server
            .bbox_knowledge(Parameters(KnowledgeListParams {
                query: Some("NO_SUCH_ENTRY_MARKER".into()),
                ..Default::default()
            }))
            .await
            .structured_content
            .expect("bbox_knowledge structured response");
        assert!(knowledge_rows(&empty).is_empty(), "{empty}");
        assert!(
            !knowledge_diagnostics(&empty)
                .iter()
                .any(|diagnostic| diagnostic.contains("legacy_compatibility")),
            "a response with no legacy rows must not warn about them: {empty}"
        );
    }
}
