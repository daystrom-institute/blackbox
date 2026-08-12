use std::collections::{BTreeMap, BTreeSet};
use anyhow::Context;

use crate::knowledge::{
    DecideParams, ForgetParams, KnowledgeLinkParams, KnowledgeListParams, LearnParams,
    RememberParams, ResponseFormat,
};
use crate::packets::packet_matches_query;
use crate::server::BlackboxServer;
use crate::system_memory;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::json;

const DEFAULT_PACKET_SIDECAR_LIMIT: usize = 8;
const DEFAULT_SYSTEM_MEMORY_SIDECAR_LIMIT: usize = 6;

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
fn rescope_project_filter(server: &crate::server::BlackboxServer, p: &mut KnowledgeListParams) {
    use bbox_corpus_core::project_selector::{ProjectResolution, ResolvedAttachment};
    let Some(raw) = p.project.as_deref() else {
        return;
    };
    // Filter-class engine resolution (phase-2 §9.2): a selector that
    // resolves rewrites to the durable store key (worktree/subdir/alias/id →
    // registered base); one that does not keeps its substring-filter
    // semantics untouched. A worktree checkout is recorded in
    // `project_alias` so entries written from inside it stay visible.
    let Some(resolution) = server.resolve_project_filter(raw) else {
        return;
    };
    // Catalog-mode ledger arm (plan §8.2): path-only entries still keyed under
    // one of this project's historical paths stay visible after attachment
    // relocation stopped rewriting them. Empty in bridge mode.
    if let Some(project_id) = resolution.project_id() {
        p.project_ledger_paths = server.ledger_historical_paths(project_id);
    }
    let ProjectResolution::Attached(ctx) = resolution else {
        return;
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

    /// Enqueue the exact committed-file bytes for a knowledge entry,
    /// delivered to the checkout by the collector. Mirrors
    /// enqueue_gap_file; the observation lane is the knowledge one.
    fn enqueue_knowledge_entry(
        &self,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
        entry: &crate::knowledge::KnowledgeEntry,
        mode: &str,
        reason: &str,
    ) -> anyhow::Result<String> {
        let relative_path = format!(".bbox/knowledge/{}.json", entry.id);
        let content = match mode {
            "write" => Some(String::from_utf8(
                crate::knowledge::committed_knowledge_entry_bytes(entry)?,
            )?),
            "delete" => None,
            other => anyhow::bail!("unknown knowledge mutation mode {other}"),
        };
        let mutation_id = self
            .state
            .checkout_mutations
            .write()
            .enqueue_file_mutation(
                scope,
                relative_path.clone(),
                mode,
                content,
                reason.to_string(),
                bbox_util::util::now_iso(),
            )?;
        self.state.checkout_mutations_persister.request();
        self.observe_knowledge_transport_operation(
            project_id,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectKnowledgeMutation,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
        );
        Ok(format!(
            "queued {mutation_id} -> {relative_path}; it lands in the checkout within one collector cycle and publishes when committed"
        ))
    }

    /// Load one entry from the served view for a covered-project mutation.
    fn served_knowledge_entry(
        &self,
        raw: &str,
        id: &str,
    ) -> anyhow::Result<crate::knowledge::KnowledgeEntry> {
        let view = self.session_knowledge_view(Some(raw), None)?;
        view.knowledge
            .all_entries()
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .with_context(|| format!("knowledge entry not found in the served view: {id}"))
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

    /// Id-addressed coverage lookup for knowledge mutations that carry no
    /// project selector (forget, review, link, learn-by-id): the served
    /// entry's stamped project id decides. Entries the published view does
    /// not serve fall through to the store path unchanged.
    fn covered_knowledge_entry_scope(
        &self,
        id: &str,
    ) -> anyhow::Result<
        Option<(
            crate::knowledge::KnowledgeEntry,
            String,
            bbox_corpus_core::identity::PublishedScope,
        )>,
    > {
        let id = id.strip_prefix("knowledge:").unwrap_or(id);
        let view = self.session_knowledge_view(None, None)?;
        let Some(entry) = view
            .knowledge
            .all_entries()
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        let Some(project_id) = entry.project_id.clone() else {
            return Ok(None);
        };
        let Some(scope) = self.covered_scope_for_project_id(&project_id) else {
            return Ok(None);
        };
        Ok(Some((entry, project_id, scope)))
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
        let Some((mut entry, project_id, scope)) = self.covered_knowledge_entry_scope(id)?
        else {
            return Ok(None);
        };
        self.patch_entry_from_learn(&mut entry, p)?;
        let delivery = self.enqueue_knowledge_entry(
            &project_id,
            scope,
            &entry,
            "write",
            &format!("bbox_learn update (project_id={project_id})"),
        )?;
        Ok(Some((
            format!(
                "Updated entry {} via the checkout-owner lane ({delivery})",
                entry.id
            ),
            entry.id.clone(),
        )))
    }

    /// Covered-project review (approve/reject), id-addressed.
    pub(crate) fn enqueue_review_via_checkout_owner(
        &self,
        action: &str,
        id: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some((mut entry, project_id, scope)) = self.covered_knowledge_entry_scope(id)?
        else {
            return Ok(None);
        };
        entry.updated_at = bbox_util::util::now_iso();
        match action {
            "approve" => {
                entry.approval = crate::knowledge::Approval::UserConfirmed;
                let delivery = self.enqueue_knowledge_entry(
                    &project_id,
                    scope,
                    &entry,
                    "write",
                    &format!("bbox_review approve (project_id={project_id})"),
                )?;
                Ok(Some(format!(
                    "Approved entry {} via the checkout-owner lane ({delivery})",
                    entry.id
                )))
            }
            "reject" => {
                entry.status = crate::knowledge::Status::Deleted;
                let delivery = self.enqueue_knowledge_entry(
                    &project_id,
                    scope,
                    &entry,
                    "delete",
                    &format!("bbox_review reject (project_id={project_id})"),
                )?;
                Ok(Some(format!(
                    "Rejected entry {} via the checkout-owner lane ({delivery})",
                    entry.id
                )))
            }
            other => anyhow::bail!("unknown review action {other}"),
        }
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
        let Some((mut entry, project_id, scope)) =
            self.covered_knowledge_entry_scope(&source_id)?
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
        let duplicate = entry.links.iter().any(|existing| {
            existing.target == edge.target
                && existing.kind == edge.kind
                && existing.source_arc == edge.source_arc
        });
        if duplicate {
            return Ok(Some(format!(
                "Link already present on {} (same target, kind, and arc)",
                entry.id
            )));
        }
        entry.links.push(edge);
        entry.updated_at = bbox_util::util::now_iso();
        let delivery = self.enqueue_knowledge_entry(
            &project_id,
            scope,
            &entry,
            "write",
            &format!("bbox_knowledge_link (project_id={project_id})"),
        )?;
        Ok(Some(format!(
            "Linked {} -> {} via the checkout-owner lane ({delivery})",
            entry.id, p.target
        )))
    }

    /// Covered-project learn: create or update through the checkout-owner
    /// backchannel, mirroring the store's field semantics.
    fn enqueue_learn_via_checkout_owner(
        &self,
        p: &LearnParams,
        raw: &str,
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
            let mut entry = self.served_knowledge_entry(raw, id)?;
            self.patch_entry_from_learn(&mut entry, p)?;
            let delivery = self.enqueue_knowledge_entry(
                project_id,
                scope,
                &entry,
                "write",
                &format!("bbox_learn update (project_id={project_id})"),
            )?;
            return Ok(format!(
                "Updated entry {} via the checkout-owner lane ({delivery})",
                entry.id
            ));
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
        let id = self.mint_knowledge_id(raw)?;
        let entry = crate::knowledge::KnowledgeEntry { id, ..entry };
        let delivery = self.enqueue_knowledge_entry(
            project_id,
            scope,
            &entry,
            "write",
            &format!("bbox_learn create (project_id={project_id})"),
        )?;
        Ok(format!(
            "Created entry {} via the checkout-owner lane [render_pending=true] ({delivery})",
            entry.id
        ))
    }

    /// Mint a knowledge id colliding with nothing in the served view.
    fn mint_knowledge_id(&self, raw: &str) -> anyhow::Result<String> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let view = self.session_knowledge_view(Some(raw), None)?;
        loop {
            let mut h = DefaultHasher::new();
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .hash(&mut h);
            std::process::id().hash(&mut h);
            std::thread::current().id().hash(&mut h);
            let id = format!("{:016x}", h.finish());
            if !view.knowledge.all_entries().iter().any(|entry| entry.id == id) {
                return Ok(id);
            }
        }
    }

    /// Covered-project remember: create an indexed-only entry through the
    /// backchannel.
    fn enqueue_remember_via_checkout_owner(
        &self,
        p: &RememberParams,
        raw: &str,
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
            id: self.mint_knowledge_id(raw)?,
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
        let delivery = self.enqueue_knowledge_entry(
            project_id,
            scope,
            &entry,
            "write",
            &format!("bbox_remember (project_id={project_id})"),
        )?;
        Ok(format!(
            "Remembered entry {} via the checkout-owner lane (indexed only, not rendered) ({delivery})",
            entry.id
        ))
    }

    /// Covered-project decide: create the decision and, when superseding,
    /// enqueue the predecessor's superseded rewrite too.
    fn enqueue_decide_via_checkout_owner(
        &self,
        p: &DecideParams,
        raw: &str,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
    ) -> anyhow::Result<String> {
        if p.content.trim().is_empty() {
            anyhow::bail!("'content' is required");
        }
        if p.rationale.trim().is_empty() {
            anyhow::bail!("'rationale' is required — a decision without justification is just a command");
        }
        let priority = Self::checkout_lane_priority(p.priority.as_deref())?;
        let now = bbox_util::util::now_iso();
        let id = self.mint_knowledge_id(raw)?;
        let entry = crate::knowledge::KnowledgeEntry {
            id: id.clone(),
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
        let delivery = self.enqueue_knowledge_entry(
            project_id,
            scope.clone(),
            &entry,
            "write",
            &format!("bbox_decide (project_id={project_id})"),
        )?;
        let mut message = format!("Decided entry {id} via the checkout-owner lane ({delivery})");
        if let Some(old_id) = p.supersedes.as_deref() {
            let mut old = self.served_knowledge_entry(raw, old_id)?;
            old.status = crate::knowledge::Status::Superseded;
            old.supersedes = Some(id.clone());
            old.updated_at = now;
            let old_delivery = self.enqueue_knowledge_entry(
                project_id,
                scope,
                &old,
                "write",
                &format!("bbox_decide supersession (project_id={project_id})"),
            )?;
            message.push_str(&format!(
                "; superseded {} ({old_delivery})",
                old.id
            ));
        }
        Ok(message)
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
        let view = self.session_knowledge_view(None, None)?;
        let Some(entry) = view
            .knowledge
            .all_entries()
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        let Some(project_id) = entry.project_id.clone() else {
            return Ok(None);
        };
        let Some(scope) = self.covered_scope_for_project_id(&project_id) else {
            return Ok(None);
        };
        if let Some(by) = p.superseded_by.as_deref() {
            let mut entry = entry;
            entry.status = crate::knowledge::Status::Superseded;
            entry.supersedes = Some(by.to_string());
            entry.updated_at = bbox_util::util::now_iso();
            let delivery = self.enqueue_knowledge_entry(
                &project_id,
                scope,
                &entry,
                "write",
                &format!("bbox_forget supersede (project_id={project_id})"),
            )?;
            return Ok(Some(format!(
                "Superseded entry {} via the checkout-owner lane ({delivery})",
                entry.id
            )));
        }
        let mut entry = entry;
        entry.status = crate::knowledge::Status::Deleted;
        entry.updated_at = bbox_util::util::now_iso();
        let delivery = self.enqueue_knowledge_entry(
            &project_id,
            scope,
            &entry,
            "delete",
            &format!("bbox_forget (project_id={project_id})"),
        )?;
        Ok(Some(format!(
            "Removed entry {} via the checkout-owner lane ({delivery})",
            entry.id
        )))
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

fn exact_system_memory_response(p: &KnowledgeListParams) -> Option<String> {
    if has_runtime_knowledge_filter(p) {
        return None;
    }
    if matches_system_memory_catalog(p.category.as_deref()) {
        return Some(format_system_memory_catalog(p.query.as_deref()));
    }
    if p.category.is_some() {
        return None;
    }
    let memory = system_memory::exact_query(p.query.as_deref())?;
    let mut out = String::new();
    out.push_str("── System memories ──────────────────────────\n");
    out.push_str(&system_memory::format_for_listing(memory));
    Some(out)
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
            return match delivered {
                Ok(message) => Self::ok_text(&message),
                Err(e) => Self::err_text(&format!("Error: {e:#}")),
            };
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
            Ok::<_, anyhow::Error>((result, rider, overlay_refreshed))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider, overlay_refreshed)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
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
            return match delivered {
                Ok(message) => Self::ok_text(&message),
                Err(e) => Self::err_text(&format!("Error: {e:#}")),
            };
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
            return match delivered {
                Ok(message) => Self::ok_text(&message),
                Err(e) => Self::err_text(&format!("Error: {e:#}")),
            };
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
        description = "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces matching rule-packets and system memories; system memories include system_memory:<id> refs usable with bbox_inspect_entity or bbox_bundle_evidence. Pass category=\"packet\" to list compiled packets, category=\"system_memory\" to list memory metadata, or bbox_packet_list for structured packet filters."
    )]
    pub(crate) async fn bbox_knowledge(
        &self,
        Parameters(p): Parameters<KnowledgeListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_knowledge", move || {
            if let Some(out) = exact_system_memory_response(&p) {
                return Ok((out.clone(), json!({ "text": out })));
            }

            let mut p = p;
            if p.project.is_some() {
                rescope_project_filter(&server, &mut p);
            }

            let mut view = server.session_knowledge_view(
                p.project.as_deref(),
                p.provisional.as_deref(),
            )?;
            let mut combined = view.knowledge.list(&p)?;
            let returned_ids = returned_entry_ids(&combined);
            if let Some(diagnostics) = view.diagnostics_text() {
                combined.push_str("\n\n");
                combined.push_str(&diagnostics);
                combined.push('\n');
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
            // (~40KB each) overflow the token budget. The agent pulls a full
            // body via the exact-id short-circuit (`bbox_knowledge(query="sm-…")`,
            // handled above by exact_system_memory_response).
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
                    "  (signposts only — query an exact sm-* id for the full runbook body)\n",
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
            let structured = view.structured_response(&returned_ids);
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
                return Ok::<_, anyhow::Error>((text, false));
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
                target.checkout.is_some(),
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge link task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((text, _provisional)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
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
                return Ok::<_, anyhow::Error>((message, p.id.clone(), false));
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
            Ok::<_, anyhow::Error>((message, target.id, target.checkout.is_some()))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge forget task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((message, id, provisional)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
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

        // Substring filter (not an absolute path) is untouched.
        let mut p = KnowledgeListParams {
            project: Some("transcript-search".into()),
            ..Default::default()
        };
        rescope_project_filter(&server, &mut p);
        assert_eq!(p.project.as_deref(), Some("transcript-search"));
        assert_eq!(p.project_alias, None);

        // An absolute path no registered project owns is untouched.
        let stranger = tmp_root.join("stranger");
        std::fs::create_dir_all(&stranger).unwrap();
        let mut p = KnowledgeListParams {
            project: Some(stranger.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_project_filter(&server, &mut p);
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

        // A registered alias rewrites to the base canonical path.
        let mut p = KnowledgeListParams {
            project: Some("blackbox".into()),
            ..Default::default()
        };
        rescope_project_filter(&server, &mut p);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.project_alias, None);

        // A project_id selector rewrites the same way.
        let mut p = KnowledgeListParams {
            project: Some(record.project_id.clone()),
            ..Default::default()
        };
        rescope_project_filter(&server, &mut p);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
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
}
