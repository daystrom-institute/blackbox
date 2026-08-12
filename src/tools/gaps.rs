use crate::gaps::{GapFileParams, GapListParams, GapResolveParams, GapUpdateParams};
use anyhow::Context;
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::gaps_tools()
}

impl BlackboxServer {
    fn guard_workspace_bound_project_gap(&self, scope: Option<&str>) -> anyhow::Result<()> {
        if self.authoritative_session_workspace_binding().is_some() && scope != Some("global") {
            anyhow::bail!(
                "error.knowledge_transport_authoritative: a bound workspace must write project gaps through its checkout-owner transport"
            );
        }
        Ok(())
    }

    fn guard_workspace_bound_gap_id(&self, raw_id: &str) -> anyhow::Result<()> {
        if self.authoritative_session_workspace_binding().is_none() {
            return Ok(());
        }
        let canonical = if raw_id.starts_with("gap-") {
            raw_id.to_string()
        } else {
            format!("gap-{raw_id}")
        };
        if self
            .state
            .gaps
            .read()
            .all()
            .iter()
            .any(|gap| gap.id == canonical && gap.project.is_none())
        {
            return Ok(());
        }
        anyhow::bail!(
            "error.knowledge_transport_authoritative: a bound workspace may mutate only global gaps through the daemon"
        )
    }

    /// Resolve a raw project path/id to its durable gap scope and optional
    /// committed-file write target — for filing and for the resolve/update
    /// rewrites alike. Delegates to the store-shared resolution in
    /// [`BlackboxServer::resolve_project_write_scope`] (`src/tools/scope.rs`).
    fn resolve_gap_project(
        &self,
        raw: &str,
    ) -> anyhow::Result<(
        String,
        Option<String>,
        Option<String>,
        Option<bbox_corpus_core::project_record::ResolvedCheckoutScope>,
    )> {
        let resolution = self.resolve_project_write(raw)?;
        let durable_scope = resolution.durable_scope;
        let resolved_project_id = resolution.project_id;
        if let Some(project_id) = resolved_project_id.as_deref().filter(|project_id| {
            self.state
                .knowledge_transport_cutover
                .covers_project_str(project_id)
        }) {
            self.observe_knowledge_transport_operation(
                project_id,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectGapMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::AuthoritativeRefusal,
            );
            anyhow::bail!(
                "error.knowledge_transport_authoritative: covered project gaps must be mutated by the checkout-owner harness"
            );
        }
        if let Some(project_id) = resolved_project_id.as_deref() {
            self.observe_knowledge_transport_operation(
                project_id,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectGapMutation,
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Local,
            );
        }
        let checkout = resolution.checkout_scope;
        let write_dir = checkout
            .as_ref()
            .map(|checkout| {
                crate::server::repo_io::RepoIoAuthority::gap_checkout_carrier(
                    durable_scope.clone(),
                    checkout,
                )
                .map(|carrier| carrier.carrier_id)
            })
            .transpose()?;
        if self.path_fallback_is_cut() && checkout.is_none() {
            anyhow::bail!(
                "path-scoped project fallback is retired; project gap writes require a registered checkout with recorded repo identity"
            );
        }
        if let Some(checkout) = checkout.as_ref() {
            self.register_dark_knowledge_checkout(checkout)?;
        }
        Ok((durable_scope, resolved_project_id, write_dir, checkout))
    }

    fn guard_unscoped_gap_mutation(
        &self,
        id: &str,
        has_project_authority: bool,
    ) -> anyhow::Result<()> {
        if self.path_fallback_is_cut()
            && !has_project_authority
            && self
                .state
                .gaps
                .read()
                .all()
                .iter()
                .any(|gap| gap.id == id && gap.project.is_some())
        {
            anyhow::bail!(
                "path-scoped project fallback is retired; project gap mutation requires session checkout authority"
            );
        }
        Ok(())
    }

    /// The catalog-published scope when the selector names a
    /// cutover-covered project. Covered projects mutate repo-owned state
    /// through the checkout-owner backchannel, never through a daemon-local
    /// checkout lease (a zero-authority daemon has none).
    pub(crate) fn covered_project_scope(
        &self,
        raw: &str,
    ) -> Option<(String, bbox_corpus_core::identity::PublishedScope)> {
        let project_id = self.validate_project_selection(raw).ok()?;
        self.covered_scope_for_project_id(&project_id)
            .map(|scope| (project_id, scope))
    }

    /// Coverage and scope lookup by durable project id (for id-addressed
    /// mutations like bbox_forget, which carry no project selector).
    pub(crate) fn covered_scope_for_project_id(
        &self,
        project_id: &str,
    ) -> Option<bbox_corpus_core::identity::PublishedScope> {
        if !self
            .state
            .knowledge_transport_cutover
            .covers_project_str(project_id)
        {
            return None;
        }
        let store = self.state.project_authority.catalog_store()?.clone();
        let snapshot = store.snapshot().ok()?;
        let id = bbox_corpus_core::project_catalog::ProjectId::parse(project_id).ok()?;
        let project = snapshot.catalog().projects.get(&id)?;
        match &project.scope {
            bbox_corpus_core::project_catalog::ProjectScope::Published(scope) => {
                Some(scope.clone())
            }
            _ => None,
        }
    }

    /// Enqueue the exact committed-file bytes for checkout-owner delivery
    /// and record the transport observation. Returns the mutation id.
    fn enqueue_gap_file(
        &self,
        project_id: &str,
        scope: bbox_corpus_core::identity::PublishedScope,
        gap: &crate::gaps::GapNote,
        reason: &str,
    ) -> anyhow::Result<String> {
        let bytes = crate::gaps::committed_gap_note_bytes(gap)?;
        let content = String::from_utf8(bytes)?;
        let relative_path = format!(".bbox/gaps/{}.json", gap.id);
        let mutation_id = self
            .state
            .checkout_mutations
            .write()
            .enqueue_file_mutation(
                scope,
                relative_path.clone(),
                "write",
                Some(content),
                reason.to_string(),
                bbox_util::util::now_iso(),
            )?;
        self.state.checkout_mutations_persister.request();
        self.observe_knowledge_transport_operation(
            project_id,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1::ProjectGapMutation,
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1::Remote,
        );
        Ok(format!(
            "queued {mutation_id} -> {relative_path}; it lands in the checkout within one collector cycle and publishes when committed"
        ))
    }

    /// Covered-project gap filing through the checkout-owner backchannel.
    /// Mirrors GapStore::file_locked's validation and open-duplicate
    /// dedupe against the served view. None = project not covered; the
    /// caller takes the local-carrier path.
    fn enqueue_gap_file_via_checkout_owner(
        &self,
        p: &GapFileParams,
        raw: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some((project_id, scope)) = self.covered_project_scope(raw) else {
            return Ok(None);
        };
        if !p.allow_recurrence.unwrap_or(false) {
            let view = self.session_gap_view(Some(raw), None)?;
            let probe = GapListParams {
                dedupe_key: Some(p.dedupe_key.trim().to_string()),
                ..Default::default()
            };
            if let Some(existing) = view
                .gaps
                .query(&probe)
                .into_iter()
                .find(|gap| gap.resolution != crate::gaps::GapResolution::Addressed)
            {
                return Ok(Some(format!(
                    "Gap already open as {} (same dedupe_key); pass allow_recurrence=true to tally a recurrence, or reference {} from a follow-up",
                    existing.id, existing.id
                )));
            }
        }
        let mut envelope = serde_json::json!({
            "type": "blackbox.gap_note.v1",
            "title": p.title,
            "gap_kind": p.gap_kind,
            "domain": p.domain,
            "wanted_capability": p.wanted_capability,
            "dedupe_key": p.dedupe_key,
        });
        let object = envelope.as_object_mut().expect("object envelope");
        for (key, value) in [
            ("impact", &p.impact),
            ("blocking_level", &p.blocking_level),
            ("missing_primitive", &p.missing_primitive),
            ("fallback_used", &p.fallback_used),
            ("notes", &p.notes),
            ("suggested_owner", &p.suggested_owner),
        ] {
            if let Some(value) = value {
                object.insert(key.to_string(), value.clone().into());
            }
        }
        if let Some(evidence) = &p.evidence {
            object.insert("evidence".to_string(), evidence.clone().into());
        }
        let id = self.mint_gap_id(raw)?;
        let now = bbox_util::util::now_iso();
        let mut gap = crate::gaps::GapNote::from_envelope(&envelope, id.clone(), now)?;
        gap.project_id = Some(project_id.clone());
        gap.task_id = p.task_id.clone();
        gap.session_id = p.session_id.clone();
        gap.provider = p.provider.clone();
        gap.bro = p.bro.clone();
        gap.thread_id = p.thread_id.clone();
        let delivery = self.enqueue_gap_file(
            &project_id,
            scope,
            &gap,
            &format!("bbox_gap(project_id={project_id})"),
        )?;
        Ok(Some(format!(
            "Gap {id} filed via the checkout-owner lane ({delivery})"
        )))
    }

    /// Canonical `gap-<8hex>` comparison (the gap crate's matcher is
    /// crate-private; the shape is fixed).
    fn gap_id_matches(gap_id: &str, needle: &str) -> bool {
        let canonical = if needle.starts_with("gap-") {
            needle.to_string()
        } else {
            format!("gap-{needle}")
        };
        gap_id == canonical
    }

    /// Mint a gap id that collides with nothing visible: neither the served
    /// view nor a pending checkout mutation's target file.
    fn mint_gap_id(&self, raw: &str) -> anyhow::Result<String> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let view = self.session_gap_view(Some(raw), None)?;
        loop {
            let mut h = DefaultHasher::new();
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .hash(&mut h);
            std::process::id().hash(&mut h);
            std::thread::current().id().hash(&mut h);
            let id = format!("gap-{:08x}", h.finish() as u32);
            if !view
                .gaps
                .all()
                .iter()
                .any(|gap| Self::gap_id_matches(&gap.id, &id))
            {
                return Ok(id);
            }
        }
    }

    /// Load one gap from the served view for a covered-project mutation.
    fn served_gap(&self, raw: &str, id: &str) -> anyhow::Result<crate::gaps::GapNote> {
        let view = self.session_gap_view(Some(raw), None)?;
        view.gaps
            .all()
            .iter()
            .find(|gap| Self::gap_id_matches(&gap.id, id))
            .cloned()
            .with_context(|| format!("Gap not found: {id} (expected `gap-<8hex>`)"))
    }

    /// Covered-project gap update: patch the served record exactly like
    /// GapStore::update_locked and enqueue the rewritten file.
    fn enqueue_gap_update_via_checkout_owner(
        &self,
        p: &GapUpdateParams,
        raw: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some((project_id, scope)) = self.covered_project_scope(raw) else {
            return Ok(None);
        };
        let mut gap = self.served_gap(raw, &p.id)?;
        if let Some(value) = p.title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            gap.title = value.to_string();
        }
        if let Some(value) = p.domain.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            gap.domain = value.to_string();
        }
        if let Some(value) = p
            .wanted_capability
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            gap.wanted_capability = value.to_string();
        }
        if let Some(value) = &p.impact {
            let mut envelope = serde_json::json!({
                "type": "blackbox.gap_note.v1",
                "title": gap.title,
                "gap_kind": gap.gap_kind.as_ref(),
                "domain": gap.domain,
                "wanted_capability": gap.wanted_capability,
                "impact": value,
            });
            envelope
                .as_object_mut()
                .expect("object envelope")
                .insert("dedupe_key".to_string(), gap.dedupe_key.clone().into());
            let checked = crate::gaps::GapNote::from_envelope(&envelope, gap.id.clone(), gap.created_at.clone())?;
            gap.impact = checked.impact;
        }
        if let Some(value) = &p.blocking_level {
            let mut envelope = serde_json::json!({
                "type": "blackbox.gap_note.v1",
                "title": gap.title,
                "gap_kind": gap.gap_kind.as_ref(),
                "domain": gap.domain,
                "wanted_capability": gap.wanted_capability,
                "blocking_level": value,
            });
            envelope
                .as_object_mut()
                .expect("object envelope")
                .insert("dedupe_key".to_string(), gap.dedupe_key.clone().into());
            let checked = crate::gaps::GapNote::from_envelope(&envelope, gap.id.clone(), gap.created_at.clone())?;
            gap.blocking_level = checked.blocking_level;
        }
        if let Some(value) = &p.missing_primitive {
            gap.missing_primitive = Some(value.clone()).filter(|s| !s.trim().is_empty());
        }
        if let Some(value) = &p.fallback_used {
            gap.fallback_used = Some(value.clone()).filter(|s| !s.trim().is_empty());
        }
        if let Some(value) = &p.evidence {
            gap.evidence = value.clone();
        }
        if let Some(value) = &p.suggested_owner {
            gap.suggested_owner = Some(value.clone()).filter(|s| !s.trim().is_empty());
        }
        if let Some(value) = &p.notes {
            gap.notes = Some(value.clone()).filter(|s| !s.trim().is_empty());
        }
        gap.updated_at = bbox_util::util::now_iso();
        let id = gap.id.clone();
        let delivery = self.enqueue_gap_file(
            &project_id,
            scope,
            &gap,
            &format!("bbox_gap_update(project_id={project_id})"),
        )?;
        Ok(Some(format!(
            "Gap {id} updated via the checkout-owner lane ({delivery})"
        )))
    }

    /// Covered-project gap resolve: apply the store's transition semantics
    /// (resolution, resolved_at, note, supersession wiring) and enqueue the
    /// rewritten file(s).
    fn enqueue_gap_resolve_via_checkout_owner(
        &self,
        p: &GapResolveParams,
        raw: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some((project_id, scope)) = self.covered_project_scope(raw) else {
            return Ok(None);
        };
        let resolution = p.resolution.as_str();
        let mut gap = self.served_gap(raw, &p.id)?;
        let now = bbox_util::util::now_iso();
        gap.resolution = match resolution {
            "unresolved" => crate::gaps::GapResolution::Unresolved,
            "acknowledged" => crate::gaps::GapResolution::Acknowledged,
            "addressed" => crate::gaps::GapResolution::Addressed,
            other => anyhow::bail!(
                "resolution must be unresolved, acknowledged, or addressed, got `{other}`"
            ),
        };
        gap.updated_at = now.clone();
        gap.resolved_at = if resolution == "unresolved" {
            None
        } else {
            Some(now.clone())
        };
        if let Some(note) = p.note.as_deref() {
            gap.resolution_note = Some(note.to_string());
        }
        let mut supersessor = None;
        if let Some(by) = p.superseded_by.as_deref() {
            let mut other = self.served_gap(raw, by)?;
            gap.superseded_by = Some(other.id.clone());
            other.supersedes = Some(gap.id.clone());
            other.updated_at = now;
            supersessor = Some(other);
        }
        let id = gap.id.clone();
        let delivery = self.enqueue_gap_file(
            &project_id,
            scope.clone(),
            &gap,
            &format!("bbox_gap_resolve(project_id={project_id})"),
        )?;
        let mut text = format!("Gap {id} → {resolution} via the checkout-owner lane ({delivery})");
        if let Some(other) = supersessor {
            let other_id = other.id.clone();
            let other_delivery = self.enqueue_gap_file(
                &project_id,
                scope,
                &other,
                &format!("bbox_gap_resolve supersession link (project_id={project_id})"),
            )?;
            text.push_str(&format!("; supersessor {other_id} relinked ({other_delivery})"));
        }
        Ok(Some(text))
    }
}

#[tool_router(router = gaps_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_gap",
        description = "File a first-class substrate gap note into the repo-owned gap store."
    )]
    pub(crate) async fn bbox_gap(
        &self,
        Parameters(mut p): Parameters<GapFileParams>,
    ) -> CallToolResult {
        if let Err(error) = self.guard_workspace_bound_project_gap(p.scope.as_deref()) {
            return Self::err_text(&format!("Error: {error:#}"));
        }
        // Gap mutations are disk-authoritative: reload + full-store rewrite
        // under a flock. Run on the blocking pool, not a tokio worker.
        let server = self.clone();
        Self::run_blocking("bbox_gap", move || {
            let mut checkout = None;
            let raw_project = p
                .project
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    (p.scope.as_deref() != Some("global"))
                        .then(|| server.authoritative_session_checkout())
                        .flatten()
                        .map(|checkout| checkout.checkout_project_dir.clone())
                });
            if let Some(raw) = raw_project {
                if let Some(text) = server.enqueue_gap_file_via_checkout_owner(&p, &raw)? {
                    return Ok(text);
                }
                let (project, resolved_project_id, write_dir, resolved_checkout) =
                    server.resolve_gap_project(&raw)?;
                p.project = Some(project);
                p.project_id = resolved_project_id;
                p.write_dir = write_dir;
                checkout = resolved_checkout;
            }
            let (id, created) = server.state.gaps.write().file(&p)?;
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_gap_overlay(checkout);
            }
            if created {
                Ok(format!("Gap {id} filed (dedupe_key={})", p.dedupe_key))
            } else {
                Ok(format!(
                    "Gap already open as {id} (same dedupe_key); pass allow_recurrence=true to tally a recurrence, or reference {id} from a follow-up"
                ))
            }
        })
        .await
    }

    #[tool(
        name = "bbox_gaps",
        description = "List / filter substrate gap notes by typed fields (gap_kind, impact, blocking_level, dedupe_key, resolution, project)."
    )]
    pub(crate) fn bbox_gaps(&self, Parameters(p): Parameters<GapListParams>) -> CallToolResult {
        Self::run_with_structured("bbox_gaps", || {
            let mut p = p;
            let requested_project = p.project.clone();
            let view =
                self.session_gap_view(requested_project.as_deref(), p.provisional.as_deref())?;
            if let Some(raw) = requested_project.as_deref() {
                // Catalog-mode ledger arm (plan §8.2): path-only gaps still
                // keyed under one of this project's historical paths stay
                // visible after relocation. Empty in bridge mode.
                p.project_ledger_paths = self.filter_ledger_paths(raw);
                if let Some(base) = self.rescope_project_filter_value(raw) {
                    p.project = Some(base);
                }
            }
            let mut used_stamp_refs = Vec::<String>::new();
            let rows = view
                .gaps
                .query(&p)
                .into_iter()
                .map(|gap| {
                    let metadata = view.gaps.view_metadata(&gap.id);
                    let mut row = serde_json::to_value(gap)?;
                    let object = row
                        .as_object_mut()
                        .expect("serialized gap response row must be an object");
                    if let Some(reference) =
                        metadata.and_then(|metadata| metadata.built_from_ref.as_ref())
                    {
                        object.insert(
                            "built_from_ref".into(),
                            serde_json::Value::String(reference.clone()),
                        );
                        used_stamp_refs.push(reference.clone());
                    }
                    if let Some(lane) =
                        metadata.and_then(|metadata| metadata.compatibility_lane.as_ref())
                    {
                        object.insert(
                            "compatibility_lane".into(),
                            serde_json::Value::String(lane.clone()),
                        );
                    }
                    Ok::<_, anyhow::Error>(row)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let built_from = view.built_from_for_refs(used_stamp_refs.iter().map(String::as_str));
            let mut structured = serde_json::json!({
                "rows": rows,
                "built_from": &built_from,
                "diagnostics": &view.diagnostics,
            });
            // Bounded structured degradation for `all` (plan §10.5): each
            // checkout the survey omitted, as a typed row. Omitted entirely
            // when nothing degraded, so bridge responses and healthy catalog
            // responses keep their existing shape. `diagnostics` carries the
            // same facts as human text.
            if !view.degraded_overlays.is_empty() {
                structured["degraded"] = serde_json::json!({ "overlays": &view.degraded_overlays });
            }
            let mut rendered = if p.json.unwrap_or(false) {
                serde_json::to_string_pretty(&structured)?
            } else {
                view.gaps.list_rendered(&p)?
            };
            if !p.json.unwrap_or(false) {
                if !view.diagnostics.is_empty() {
                    rendered.push_str("\n\nProvisional gap diagnostics:\n- ");
                    rendered.push_str(&view.diagnostics.join("\n- "));
                }
                rendered = view.append_built_from_table(rendered, &built_from);
            }
            Ok((rendered, structured))
        })
    }

    #[tool(
        name = "bbox_gap_resolve",
        description = "Resolve a gap note (acknowledged/addressed); optionally wire a structured supersession link."
    )]
    pub(crate) async fn bbox_gap_resolve(
        &self,
        Parameters(mut p): Parameters<GapResolveParams>,
    ) -> CallToolResult {
        if let Err(error) = self.guard_workspace_bound_gap_id(&p.id) {
            return Self::err_text(&format!("Error: {error:#}"));
        }
        let server = self.clone();
        Self::run_blocking("bbox_gap_resolve", move || {
            // `project` is write-targeting only: resolve it through the same
            // path as filing so a recognized worktree redirects the rewritten
            // repo-owned file into the session's checkout. The gap's durable
            // project scope never changes; absent → today's behavior.
            let mut checkout = None;
            let raw_project = p
                .project
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    server
                        .authoritative_session_checkout()
                        .map(|checkout| checkout.checkout_project_dir.clone())
                });
            server.guard_unscoped_gap_mutation(&p.id, raw_project.is_some())?;
            if let Some(raw) = raw_project {
                if let Some(text) = server.enqueue_gap_resolve_via_checkout_owner(&p, &raw)? {
                    return Ok(text);
                }
                let (project, _resolved_project_id, write_dir, resolved_checkout) =
                    server.resolve_gap_project(&raw)?;
                p.project = Some(project);
                p.write_dir = write_dir;
                checkout = resolved_checkout;
            }
            let result = server.state.gaps.write().resolve(&p)?;
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_gap_overlay(checkout);
            }
            Ok(result)
        })
        .await
    }

    #[tool(
        name = "bbox_gap_update",
        description = "Edit an existing gap note's fields in place."
    )]
    pub(crate) async fn bbox_gap_update(
        &self,
        Parameters(mut p): Parameters<GapUpdateParams>,
    ) -> CallToolResult {
        if let Err(error) = self.guard_workspace_bound_gap_id(&p.id) {
            return Self::err_text(&format!("Error: {error:#}"));
        }
        let server = self.clone();
        Self::run_blocking("bbox_gap_update", move || {
            // Same write-targeting resolution as bbox_gap_resolve.
            let mut checkout = None;
            let raw_project = p
                .project
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    server
                        .authoritative_session_checkout()
                        .map(|checkout| checkout.checkout_project_dir.clone())
                });
            server.guard_unscoped_gap_mutation(&p.id, raw_project.is_some())?;
            if let Some(raw) = raw_project {
                if let Some(text) = server.enqueue_gap_update_via_checkout_owner(&p, &raw)? {
                    return Ok(text);
                }
                let (project, _resolved_project_id, write_dir, resolved_checkout) =
                    server.resolve_gap_project(&raw)?;
                p.project = Some(project);
                p.write_dir = write_dir;
                checkout = resolved_checkout;
            }
            let result = server.state.gaps.write().update(&p)?;
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_gap_overlay(checkout);
            }
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn bind_remote_workspace(server: &BlackboxServer) {
        assert!(
            server
                .session_workspace_binding
                .set(Some(Arc::new(
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
    fn bound_workspace_daemon_lane_accepts_only_global_gap_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(directory.path())));
        bind_remote_workspace(&server);

        assert!(
            server
                .guard_workspace_bound_project_gap(Some("global"))
                .is_ok()
        );
        assert!(
            server
                .guard_workspace_bound_project_gap(Some("project"))
                .unwrap_err()
                .to_string()
                .contains("knowledge_transport_authoritative")
        );
        assert!(
            server
                .guard_workspace_bound_gap_id("gap-1234abcd")
                .unwrap_err()
                .to_string()
                .contains("only global gaps")
        );
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    fn gap_params(project: String) -> GapFileParams {
        GapFileParams {
            title: "worktree gap".into(),
            gap_kind: "tooling".into(),
            domain: "test-domain".into(),
            wanted_capability: "write into the worktree".into(),
            dedupe_key: "tooling/test-domain/worktree-gap".into(),
            impact: None,
            blocking_level: None,
            missing_primitive: None,
            fallback_used: None,
            evidence: None,
            suggested_owner: None,
            notes: None,
            scope: Some("project".into()),
            project: Some(project),
            project_id: None,
            write_dir: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            allow_recurrence: None,
        }
    }

    /// Init a committed git repo at `dir` and return its canonical path.
    fn init_repo(dir: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "t@example.com"]);
        run_git(dir, &["config", "user.name", "T"]);
        std::fs::write(dir.join("README.md"), "base").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "init"]);
        crate::config::ensure_recorded_repo_id(dir).unwrap();
        run_git(dir, &["add", ".bbox/config.toml"]);
        run_git(dir, &["commit", "-m", "record project identity"]);
        dir.canonicalize().unwrap()
    }

    /// Server with `base` registered, one gap filed at the base, and the base
    /// wired as a loaded gap root (mirrors daemon boot) so mutations reload it.
    async fn server_with_base_gap(
        state_dir: &Path,
        base_canon: &std::path::PathBuf,
    ) -> (BlackboxServer, String) {
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(state_dir)));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(base_canon)
            .unwrap();
        let projects = server.state.records_provider.records_snapshot().records;
        let repo_io = std::sync::Arc::new(crate::server::repo_io::RepoIoAuthority::new(
            server.state.checkout_access.clone(),
        ));
        server
            .state
            .gaps
            .write()
            .configure_repo_io(
                repo_io.clone(),
                repo_io,
                crate::server::repo_io::RepoIoAuthority::gap_base_carriers(&projects, None)
                    .unwrap(),
            )
            .unwrap();
        let filed = server
            .bbox_gap(Parameters(gap_params(
                base_canon.to_string_lossy().into_owned(),
            )))
            .await;
        assert_ne!(filed.is_error, Some(true), "bbox_gap failed: {filed:?}");
        let id = server.state.gaps.read().all().first().unwrap().id.clone();
        (server, id)
    }

    fn gap_file(root: &Path, id: &str) -> std::path::PathBuf {
        root.join(".bbox").join("gaps").join(format!("{id}.json"))
    }

    #[tokio::test]
    async fn path_cut_rejects_unregistered_project_gap_write() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("unregistered");
        std::fs::create_dir_all(&project).unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .path_fallback_cut
            .store(true, std::sync::atomic::Ordering::Release);
        let result = server
            .bbox_gap(Parameters(gap_params(
                project.to_string_lossy().into_owned(),
            )))
            .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert!(format!("{:?}", result.content).contains("project gap writes require"));
    }

    /// (a) resolve with project=<in-tree linked worktree>: rewritten file
    /// lands in the worktree; the base checkout's copy stays untouched.
    #[tokio::test]
    async fn bbox_gap_resolve_from_in_tree_worktree_writes_worktree_keeps_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;
        let base_before = std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap();

        // In-tree linked worktree (harness shape, arbitrary branch name).
        let wt = base.join(".claude").join("worktrees").join("wt-a");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-wt-a",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                note: Some("done on branch".into()),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );

        let wt_file = gap_file(&wt_canon, &id);
        assert!(wt_file.exists(), "resolve must write into the worktree");
        assert!(
            std::fs::read_to_string(&wt_file)
                .unwrap()
                .contains("addressed")
        );
        assert_eq!(
            std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap(),
            base_before,
            "base checkout copy must be untouched"
        );

        let updated = server
            .bbox_gap_update(Parameters(GapUpdateParams {
                id: id.clone(),
                notes: Some("follow-up in the same checkout".into()),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(updated.is_error, Some(true), "update failed: {updated:?}");
        let twice_mutated = std::fs::read_to_string(gap_file(&wt_canon, &id)).unwrap();
        assert!(twice_mutated.contains("addressed"));
        assert!(twice_mutated.contains("follow-up in the same checkout"));
    }

    /// (a, update flavor) update with project=<in-tree worktree> redirects too.
    #[tokio::test]
    async fn bbox_gap_update_from_in_tree_worktree_writes_worktree_keeps_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;
        let base_before = std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap();

        let wt = base.join(".claude").join("worktrees").join("wt-u");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-wt-u",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let updated = server
            .bbox_gap_update(Parameters(GapUpdateParams {
                id: id.clone(),
                notes: Some("amended-from-worktree".into()),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(updated.is_error, Some(true), "update failed: {updated:?}");

        let wt_file = gap_file(&wt_canon, &id);
        assert!(wt_file.exists(), "update must write into the worktree");
        assert!(
            std::fs::read_to_string(&wt_file)
                .unwrap()
                .contains("amended-from-worktree")
        );
        assert_eq!(
            std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap(),
            base_before,
            "base checkout copy must be untouched"
        );
    }

    /// (b) resolve with project=<out-of-tree bro-fleet worktree>.
    #[tokio::test]
    async fn bbox_gap_resolve_from_fleet_worktree_writes_worktree_keeps_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;
        let base_before = std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap();

        let wt = tmp.path().join("wt-fleet");
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/x",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );

        let wt_file = gap_file(&wt_canon, &id);
        assert!(
            wt_file.exists(),
            "resolve must write into the fleet worktree"
        );
        assert!(
            std::fs::read_to_string(&wt_file)
                .unwrap()
                .contains("addressed")
        );
        assert_eq!(
            std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap(),
            base_before,
            "base checkout copy must be untouched"
        );
    }

    /// (c) project absent → today's behavior: the base copy is rewritten.
    /// (d) a plain subdirectory of the root is NEVER worktree-classed: the
    /// rewrite still lands at the base and no `.bbox/` appears in the subdir.
    #[tokio::test]
    async fn bbox_gap_resolve_without_worktree_rewrites_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;

        // (c) absent project.
        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "acknowledged".into(),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );
        assert!(
            std::fs::read_to_string(gap_file(&base_canon, &id))
                .unwrap()
                .contains("acknowledged"),
            "absent project must rewrite the base copy"
        );

        // (d) plain subdirectory passed as project.
        let subdir = base_canon.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                project: Some(subdir.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );
        assert!(
            std::fs::read_to_string(gap_file(&base_canon, &id))
                .unwrap()
                .contains("addressed"),
            "plain subdir project must still rewrite the base copy"
        );
        assert!(
            !subdir.join(".bbox").exists(),
            "a plain subdirectory must never be worktree-classed"
        );
    }

    /// (e) global-scope gap mutation ignores the project param entirely.
    #[tokio::test]
    async fn bbox_gap_resolve_global_gap_ignores_project_param() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap();

        let mut p = gap_params(String::new());
        p.scope = Some("global".into());
        // A defaulted project must not project-scope a global filing.
        p.project = Some(base_canon.to_string_lossy().into_owned());
        let filed = server.bbox_gap(Parameters(p)).await;
        assert_ne!(filed.is_error, Some(true), "bbox_gap failed: {filed:?}");
        let id = server.state.gaps.read().all().first().unwrap().id.clone();
        assert!(
            server.state.gaps.read().all()[0].project.is_none(),
            "scope=global must win over a supplied project"
        );

        let wt = base.join(".claude").join("worktrees").join("wt-g");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-wt-g",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );

        assert!(
            !gap_file(&wt_canon, &id).exists(),
            "global gap mutation must not write into the worktree"
        );
        assert!(
            !gap_file(&base_canon, &id).exists(),
            "global gap must stay in the central store"
        );
        let gaps = server.state.gaps.read();
        let gap = gaps.all().iter().find(|g| g.id == id).unwrap();
        assert_eq!(gap.resolution, crate::gaps::GapResolution::Addressed);
        assert!(
            gap.write_dir.is_none(),
            "global gap must ignore write-targeting"
        );
    }

    #[tokio::test]
    async fn bbox_gap_from_worktree_keys_base_writes_worktree_and_list_normalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();
        crate::config::ensure_recorded_repo_id(&base_canon).unwrap();
        run_git(&base, &["add", ".bbox/config.toml"]);
        run_git(&base, &["commit", "-m", "record identity"]);

        let worktree = tmp.path().join("wt");
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/x",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let worktree_canon = worktree.canonicalize().unwrap();
        let wt = worktree_canon.to_string_lossy().into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap();

        let filed = server.bbox_gap(Parameters(gap_params(wt.clone()))).await;
        assert_ne!(filed.is_error, Some(true), "bbox_gap failed: {filed:?}");

        assert!(
            server.state.gaps.read().all().is_empty(),
            "checkout gap must not be retained in the central store"
        );
        let row = server
            .state
            .checkout_registry
            .read()
            .rows()
            .iter()
            .find(|row| row.checkout_dir == wt)
            .cloned()
            .expect("worktree registered for gap overlay");
        let scope = row.published_scope().unwrap();
        let snapshot = server
            .state
            .gap_overlays
            .read()
            .get(&scope, &row.checkout_id)
            .cloned()
            .expect("gap overlay published");
        let id = snapshot.values.keys().next().unwrap().clone();
        server.set_session_checkout_for_test(
            row.project_id.clone().expect("checkout row project id"),
            scope,
            row.checkout_id.clone(),
            worktree_canon.clone(),
        );
        let gap = {
            let view = server.session_gap_view(Some(&wt), Some("own")).unwrap();
            view.gaps.all().first().expect("one own gap").clone()
        };
        assert_eq!(
            gap.project.as_deref(),
            Some(base_canon.to_string_lossy().as_ref()),
            "logical scope must be the registered base"
        );
        assert!(
            gap.write_dir.is_none(),
            "read view must not expose a host-local redirect"
        );
        assert_eq!(
            gap.provisional_checkout_id.as_deref(),
            Some(row.checkout_id.as_str())
        );
        assert!(
            worktree_canon
                .join(".bbox")
                .join("gaps")
                .join(format!("{id}.json"))
                .exists(),
            "gap should be written into the worktree"
        );
        assert!(
            !base_canon
                .join(".bbox")
                .join("gaps")
                .join(format!("{id}.json"))
                .exists(),
            "gap must not be written into the base checkout"
        );

        let updated = server
            .bbox_gap_update(Parameters(GapUpdateParams {
                id: id.clone(),
                notes: Some("session-authoritative update".into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(updated.is_error, Some(true), "update failed: {updated:?}");
        assert!(
            std::fs::read_to_string(gap_file(&worktree_canon, &id))
                .unwrap()
                .contains("session-authoritative update")
        );

        let published = server.bbox_gaps(Parameters(GapListParams {
            project: Some(wt.clone()),
            provisional: Some("published".into()),
            include_addressed: Some(true),
            ..Default::default()
        }));
        assert_ne!(
            published.is_error,
            Some(true),
            "published list failed: {published:?}"
        );
        assert!(format!("{:?}", published.content).contains("No gaps found"));

        let list = server.bbox_gaps(Parameters(GapListParams {
            project: Some(wt),
            provisional: Some("own".into()),
            include_addressed: Some(true),
            ..Default::default()
        }));
        assert_ne!(list.is_error, Some(true), "bbox_gaps failed: {list:?}");
        let body = format!("{:?}", list.content);
        assert!(
            body.contains("worktree gap"),
            "worktree-scoped list should find the base-keyed gap: {body}"
        );
        assert!(body.contains("built_from=built_from_"), "{body}");
        assert!(body.contains("working_fingerprint="), "{body}");
        let structured = list
            .structured_content
            .expect("bbox_gaps structured response");
        let reference = structured["rows"][0]["built_from_ref"]
            .as_str()
            .expect("row stamp reference");
        assert!(structured["built_from"].get(reference).is_some());

        let json_list = server.bbox_gaps(Parameters(GapListParams {
            project: Some(worktree_canon.to_string_lossy().into_owned()),
            provisional: Some("own".into()),
            include_addressed: Some(true),
            json: Some(true),
            ..Default::default()
        }));
        assert_ne!(
            json_list.is_error,
            Some(true),
            "json list failed: {json_list:?}"
        );
        let json_body = format!("{:?}", json_list.content);
        assert!(json_body.contains("built_from_ref"), "{json_body}");
        assert!(json_body.contains("built_from"), "{json_body}");
        assert!(json_body.contains(reference), "{json_body}");
    }
}
