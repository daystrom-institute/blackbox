use std::collections::BTreeMap;

use parking_lot::RwLock;

use crate::AGENT_QUERY_EMBED_CACHE;
use crate::artifacts;
use crate::embed;
use crate::orchestration;
use crate::orchestration::providers::ExecOpts;
use crate::server::state::BlackboxServer;
use crate::server::tail::resolve_agent_vector_search;
use crate::tools::bro_params::{
    AgentDescribeParams, AgentDispatchParams, AgentGetParams, AgentListParams, AgentSearchParams,
    AgentVectorPlan,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::agents_tools()
}

#[tool_router(router = agents_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_agent_list",
        description = "List installed agents from the registry. Optional filters for cost_class, provenance_kind, include_superseded, and limit."
    )]
    pub(crate) fn bro_agent_list(
        &self,
        Parameters(p): Parameters<AgentListParams>,
    ) -> CallToolResult {
        use orchestration::agents::registry::{AgentRegistry, ListFilter};
        use orchestration::agents::types::AgentCostClass;
        let catalog = self.state.artifacts.read();
        let reg = AgentRegistry::new(&catalog);
        let cost_class = match p.cost_class.as_deref() {
            Some(s) => {
                let parsed: AgentCostClass = match serde_json::from_value(
                    serde_json::Value::String(s.to_string()),
                ) {
                    Ok(c) => c,
                    Err(_) => {
                        return Self::err_text(&format!(
                            "unknown cost_class: {s} (expected one of: cheap, normal, expensive)"
                        ));
                    }
                };
                Some(parsed)
            }
            None => None,
        };
        let filter = ListFilter {
            include_superseded: p.include_superseded.unwrap_or(false),
            cost_class,
            provenance_kind: p.provenance_kind,
        };
        match reg.list(&filter) {
            Ok(summaries) => {
                let capped = match p.limit {
                    Some(n) => summaries.into_iter().take(n).collect::<Vec<_>>(),
                    None => summaries,
                };
                Self::ok_json(&serde_json::json!({
                    "agents": capped.iter().map(|s| {
                        let mut m = serde_json::Map::from_iter([
                            ("name".into(), serde_json::Value::String(s.name.clone())),
                            ("version".into(), serde_json::Value::String(s.version.clone())),
                            ("active".into(), serde_json::Value::Bool(s.active)),
                            ("installed_at".into(), serde_json::Value::String(s.installed_at.clone())),
                            ("embedding_pending".into(), match s.embedding_pending {
                                Some(b) => serde_json::Value::Bool(b),
                                None => serde_json::Value::Null,
                            }),
                        ]);
                        if let Some(desc) = &s.description {
                            m.insert("description".into(), serde_json::Value::String(desc.clone()));
                        }
                        if let Some(cc) = &s.cost_class {
                            m.insert("cost_class".into(), serde_json::Value::String(cc.to_string()));
                        }
                        if let Some(pk) = &s.provenance_kind {
                            m.insert("provenance_kind".into(), serde_json::Value::String(pk.clone()));
                        }
                        if !s.supersedes_chain.is_empty() {
                            m.insert(
                                "supersedes_chain".into(),
                                serde_json::Value::Array(
                                    s.supersedes_chain
                                        .iter()
                                        .map(|c| serde_json::Value::String(c.clone()))
                                        .collect(),
                                ),
                            );
                        }
                        serde_json::Value::Object(m)
                    }).collect::<Vec<_>>()
                }))
            }
            Err(e) => Self::err_text(&format!("registry list failed: {e}")),
        }
    }

    #[tool(
        name = "bro_agent_get",
        description = "Read full details for a single agent by name or agent-ref (name@vN or agent:name@vN). Returns manifest, metadata, and lifecycle state."
    )]
    pub(crate) fn bro_agent_get(
        &self,
        Parameters(p): Parameters<AgentGetParams>,
    ) -> CallToolResult {
        use orchestration::agents::registry::AgentRegistry;
        let catalog = self.state.artifacts.read();
        let reg = AgentRegistry::new(&catalog);
        match reg.get(&p.name) {
            Ok(Some(rec)) => {
                let mut m = serde_json::Map::from_iter([
                    ("name".into(), serde_json::Value::String(rec.name)),
                    ("version".into(), serde_json::Value::String(rec.version)),
                    ("active".into(), serde_json::Value::Bool(rec.active)),
                    (
                        "installed_at".into(),
                        serde_json::Value::String(rec.installed_at),
                    ),
                    ("source".into(), serde_json::Value::String(rec.source)),
                ]);
                if let Some(s) = rec.metadata.supersedes {
                    m.insert("supersedes".into(), serde_json::Value::String(s));
                }
                if !rec.metadata.supersedes_chain.is_empty() {
                    m.insert(
                        "supersedes_chain".into(),
                        serde_json::Value::Array(
                            rec.metadata
                                .supersedes_chain
                                .into_iter()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
                if let Some(s) = rec.metadata.superseded_by {
                    m.insert("superseded_by".into(), serde_json::Value::String(s));
                }
                if let Some(parse_err) = rec.manifest_parse_error {
                    m.insert(
                        "manifest_parse_error".into(),
                        serde_json::Value::String(parse_err),
                    );
                }
                if let Some(manifest) = rec.manifest {
                    m.insert(
                        "manifest".into(),
                        serde_json::to_value(manifest).unwrap_or_else(|e| {
                            serde_json::Value::String(format!("<serialize error: {e}>"))
                        }),
                    );
                }
                Self::ok_json(&serde_json::Value::Object(m))
            }
            Ok(None) => Self::err_text(&format!("agent not found: {}", p.name)),
            Err(e) => Self::err_text(&format!("registry get failed: {e}")),
        }
    }

    pub(crate) fn expand_template(template: &str, args: &serde_json::Value) -> String {
        let mut result = template.to_string();
        if let Some(obj) = args.as_object() {
            for (key, value) in obj {
                let pattern = format!("{{{{{}}}}}", key);
                let replacement = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                result = result.replace(&pattern, &replacement);
            }
        }
        result
    }

    fn validate_operator_authority_inputs(
        manifest: &orchestration::agents::types::AgentManifest,
        args: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        const OPERATOR_AUTHORITY_FLAGS: [&str; 2] =
            ["acknowledge_repr", "acknowledge_public_api_change"];

        let schema_properties = manifest
            .inputs
            .as_ref()
            .and_then(|inputs| inputs.schema.as_ref())
            .and_then(|schema| schema.get("properties"))
            .and_then(|properties| properties.as_object());
        let prompt_template = manifest
            .inputs
            .as_ref()
            .and_then(|inputs| inputs.prompt_template.as_deref())
            .unwrap_or_default();

        for flag in OPERATOR_AUTHORITY_FLAGS {
            let declared_input = schema_properties
                .map(|properties| properties.contains_key(flag))
                .unwrap_or(false);

            if args.get(flag).is_some() && !declared_input {
                return Err(format!(
                    "error.bad_input(code=operator_authority_flag_not_declared): \
                     `{flag}` may only be passed through a declared agent input"
                ));
            }
            let placeholder = format!("{{{{{}}}}}", flag);
            if prompt_template.contains(&placeholder) && !declared_input {
                return Err(format!(
                    "error.bad_input(code=operator_authority_flag_not_declared): \
                     prompt template references `{flag}` but inputs.schema.properties does not declare it"
                ));
            }
            let quoted = format!("\"{flag}\": true");
            let compact_quoted = format!("\"{flag}\":true");
            let bare = format!("{flag}: true");
            let compact_bare = format!("{flag}:true");
            if prompt_template.contains(&quoted)
                || prompt_template.contains(&compact_quoted)
                || prompt_template.contains(&bare)
                || prompt_template.contains(&compact_bare)
            {
                return Err(format!(
                    "error.bad_input(code=operator_authority_flag_constant): \
                     prompt template hardcodes `{flag}=true`; operator-authority flags must be supplied by inputs"
                ));
            }
        }

        Ok(())
    }

    pub(crate) fn embed_agent_query(query: &str) -> anyhow::Result<Vec<f32>> {
        let router = embed::EmbeddingRouter::load_default()?;
        let route = router.route(embed::Bucket::AgentManifest, None)?;
        let cache_key = format!("{}:{}:{}", route.provider_id, route.model, query);
        let cache = AGENT_QUERY_EMBED_CACHE.get_or_init(|| RwLock::new(BTreeMap::new()));
        if let Some(vector) = cache.read().get(&cache_key).cloned() {
            return Ok(vector);
        }
        let provider = router.route_for(embed::Bucket::AgentManifest, None)?;
        let texts = vec![query.to_string()];
        let vectors = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| handle.block_on(provider.embed_batch(&texts)))
            }
            Err(_) => {
                let runtime = tokio::runtime::Runtime::new()?;
                runtime.block_on(provider.embed_batch(&texts))
            }
        }?;
        let vector = vectors
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("embedding provider returned no query vector"))?;
        let mut guard = cache.write();
        if guard.len() >= 256 {
            if let Some(first) = guard.keys().next().cloned() {
                guard.remove(&first);
            }
        }
        guard.insert(cache_key, vector.clone());
        Ok(vector)
    }

    pub(crate) fn extract_inline_filters(inline: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        let filters = match inline.get("filters") {
            Some(f) => f,
            None => return (Vec::new(), Vec::new()),
        };
        let allow = filters
            .get("allow")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let disallow = filters
            .get("disallow")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        (allow, disallow)
    }

    #[tool(
        name = "bro_agent_describe",
        description = "Full manifest + resolved brofile + merged filters for one agent. Returns the computed dispatch surface (deny-wins filter merge of brofile + overlay), brofile info, embedding status, and any warnings."
    )]
    pub(crate) fn bro_agent_describe(
        &self,
        Parameters(p): Parameters<AgentDescribeParams>,
    ) -> CallToolResult {
        use orchestration::agents::registry::AgentRegistry;
        use orchestration::agents::types::MergedFilters;
        let catalog = self.state.artifacts.read();
        let reg = AgentRegistry::new(&catalog);
        let rec = match reg.get(&p.agent) {
            Ok(Some(r)) => r,
            Ok(None) => return Self::err_text(&format!("agent not found: {}", p.agent)),
            Err(e) => return Self::err_text(&format!("registry get failed: {e}")),
        };
        let manifest = match rec.manifest {
            Some(m) => m,
            None => {
                return Self::ok_json(&serde_json::json!({
                    "name": rec.name,
                    "version": rec.version,
                    "active": rec.active,
                    "error": format!("manifest parse failed: {}", rec.manifest_parse_error.unwrap_or_default()),
                }));
            }
        };

        let mut warnings: Vec<String> = Vec::new();
        let mut degraded = serde_json::Map::new();

        let (brofile_kind, brofile_name, brofile_provider, brofile_body, base_allow, base_disallow) =
            if let Some(ref br) = manifest.brofile_ref {
                if let Ok(Some(meta)) = catalog.metadata_for(artifacts::ArtifactKind::Brofile, br) {
                    if !meta.active {
                        degraded.insert("manifest_stale".into(), serde_json::Value::Bool(true));
                        warnings.push(format!(
                            "brofile_ref '{br}' is superseded by {}; reinstall or upgrade the agent manifest",
                            meta.superseded_by.unwrap_or_else(|| "unknown".into())
                        ));
                    }
                }
                let resolved =
                    orchestration::brofile::resolve_brofile(br, &self.state.store_dir, None);
                match resolved {
                    Some(bf) => {
                        let (ba, bd) = match &bf.filters {
                            Some(f) => (f.allow.clone(), f.disallow.clone()),
                            None => (Vec::new(), Vec::new()),
                        };
                        (
                            "ref",
                            br.clone(),
                            Some(bf.provider.as_str().to_string()),
                            Some(serde_json::to_value(&bf).unwrap_or(serde_json::Value::Null)),
                            ba,
                            bd,
                        )
                    }
                    None => {
                        warnings.push(format!(
                            "brofile_ref '{br}' not found (global scope only; project-scoped brofiles not yet supported by describe)"
                        ));
                        ("ref", br.clone(), None, None, Vec::new(), Vec::new())
                    }
                }
            } else if let Some(ref inline) = manifest.brofile_inline {
                let prov = inline
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let (ba, bd) = Self::extract_inline_filters(inline);
                (
                    "inline",
                    String::new(),
                    Some(prov.to_string()),
                    Some(inline.clone()),
                    ba,
                    bd,
                )
            } else {
                warnings.push("manifest has neither brofile_ref nor brofile_inline".into());
                ("none", String::new(), None, None, Vec::new(), Vec::new())
            };

        let merged = MergedFilters::merge(
            &base_allow,
            &base_disallow,
            manifest.filter_overlay.as_ref(),
        );

        let embedding_status = match manifest.embedding {
            Some(_) => "embedded",
            None => "pending",
        };
        let install_warnings = rec
            .metadata
            .install_warnings
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect::<Vec<_>>();

        let mut result = serde_json::Map::from_iter([
            ("name".into(), serde_json::Value::String(rec.name)),
            ("version".into(), serde_json::Value::String(rec.version)),
            ("active".into(), serde_json::Value::Bool(rec.active)),
            (
                "embedding_status".into(),
                serde_json::Value::String(embedding_status.to_string()),
            ),
            (
                "brofile_kind".into(),
                serde_json::Value::String(brofile_kind.to_string()),
            ),
            (
                "merged_filters".into(),
                serde_json::to_value(&merged).unwrap_or(serde_json::Value::Null),
            ),
            (
                "install_warnings".into(),
                serde_json::Value::Array(install_warnings),
            ),
        ]);
        if !brofile_name.is_empty() {
            result.insert(
                "brofile_name".into(),
                serde_json::Value::String(brofile_name),
            );
        }
        if let Some(provider) = brofile_provider {
            result.insert(
                "brofile_provider".into(),
                serde_json::Value::String(provider),
            );
        }
        if let Some(body) = brofile_body {
            result.insert("brofile".into(), body);
        }
        if !warnings.is_empty() {
            result.insert(
                "warnings".into(),
                serde_json::Value::Array(
                    warnings
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        if !degraded.is_empty() {
            result.insert("degraded".into(), serde_json::Value::Object(degraded));
        }
        result.insert(
            "manifest".into(),
            serde_json::to_value(&manifest).unwrap_or(serde_json::Value::Null),
        );
        Self::ok_json(&serde_json::Value::Object(result))
    }

    #[tool(
        name = "bro_agent_search",
        description = "Search installed agents by query string. Matches against description and when_to_use; penalizes or excludes results matching anti_patterns. Returns ranked results with scores, provenance, and matched anti-patterns."
    )]
    pub(crate) fn bro_agent_search(
        &self,
        Parameters(p): Parameters<AgentSearchParams>,
    ) -> CallToolResult {
        use orchestration::agents::registry::{AgentRegistry, AgentVectorSearch, SearchFilter};
        use orchestration::agents::types::AgentCostClass;
        let query = p.query.trim();
        if query.is_empty() {
            return Self::err_text("query is required");
        }
        let limit = p.limit.unwrap_or(5).min(50) as usize;
        let cost_class = match p.cost_class.as_deref() {
            Some("cheap") => Some(AgentCostClass::Cheap),
            Some("normal") => Some(AgentCostClass::Normal),
            Some("expensive") => Some(AgentCostClass::Expensive),
            Some(other) => return Self::err_text(&format!("invalid cost_class: {other}")),
            None => None,
        };
        let filter = SearchFilter {
            cost_class,
            provenance_kind: p.provenance_kind,
        };
        let exclude_ap = p.exclude_anti_pattern_matches.unwrap_or(true);
        let catalog = self.state.artifacts.read();
        let reg = AgentRegistry::new(&catalog);
        let active_agents = match reg.list(&orchestration::agents::registry::ListFilter::default())
        {
            Ok(list) => list,
            Err(e) => return Self::err_text(&format!("registry list failed: {e}")),
        };
        let embedded_agents = active_agents
            .iter()
            .filter(|agent| agent.embedding_pending == Some(false))
            .count();
        let vector_plan = if p.include_vectors.unwrap_or(true) {
            resolve_agent_vector_search(query, p.query_vector.as_deref())
        } else {
            AgentVectorPlan {
                search: None,
                route: None,
                error: Some("vector search disabled by caller".into()),
            }
        };
        let vector_search = vector_plan.search.as_ref().map(|search| AgentVectorSearch {
            route: search.route.clone(),
            query_vector: search.query_vector.clone(),
        });
        let results = match reg.search_with_vectors(
            query,
            limit,
            &filter,
            exclude_ap,
            vector_search.as_ref(),
        ) {
            Ok(r) => r,
            Err(e) => return Self::err_text(&format!("search failed: {e}")),
        };
        let json_results: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                let mut obj = serde_json::json!({
                    "name": r.name,
                    "version": r.version,
                    "score": (r.score * 1000.0).round() / 1000.0,
                    "description": r.description,
                    "when_to_use": r.when_to_use,
                    "anti_patterns": r.anti_patterns,
                    "cost_class": r.cost_class,
                    "provenance_kind": r.provenance_kind,
                    "sources": r.sources,
                });
                if !exclude_ap {
                    obj["matched_anti_patterns"] = serde_json::json!(r.matched_anti_patterns);
                }
                obj
            })
            .collect();
        let active_count = active_agents.len();
        let vector_available = vector_plan.search.is_some();
        let coverage_ratio = if active_count == 0 {
            1.0
        } else {
            embedded_agents as f64 / active_count as f64
        };
        Self::ok_json(&serde_json::json!({
            "results": json_results,
            "search_mode": if vector_available { "hybrid" } else { "keyword" },
            "total_matched": json_results.len(),
            "active_agents": active_count,
            "degraded": {
                "embedding_pending": embedded_agents < active_count,
                "vector_search_unavailable": !vector_available,
                "vector_error": vector_plan.error,
            },
            "vector_status": {
                "coverage_ratio": coverage_ratio,
                "embedded_agents": embedded_agents,
                "active_agents": active_count,
                "route": vector_plan.route,
            },
        }))
    }

    #[tool(
        name = "bro_agent_dispatch",
        description = "Dispatch a registered agent for a focused task. Routes through manifest dispatch_adapter if set, otherwise resolves brofile, merges filters, expands prompt template, and spawns via the standard bro execution path. Returns task_id, session, and agent attribution (agentLabel on the spawned task, preserved even when bro= routes to a named team member)."
    )]
    pub(crate) async fn bro_agent_dispatch(
        &self,
        Parameters(p): Parameters<AgentDispatchParams>,
    ) -> CallToolResult {
        use orchestration::agents::adapter::DispatchContext;
        use orchestration::agents::registry::AgentRegistry;
        use orchestration::agents::types::{AgentRef, AgentSession, MergedFilters};

        let (manifest, agent_ref, bro_label) = {
            let catalog = self.state.artifacts.read();
            let reg = AgentRegistry::new(&catalog);
            let rec = match reg.get(&p.agent) {
                Ok(Some(r)) => r,
                Ok(None) => return Self::err_text(&format!("agent not found: {}", p.agent)),
                Err(e) => return Self::err_text(&format!("registry get failed: {e}")),
            };
            let manifest = match rec.manifest {
                Some(m) => m,
                None => {
                    return Self::err_text(&format!(
                        "agent '{}' has unparseable manifest: {}",
                        p.agent,
                        rec.manifest_parse_error.unwrap_or_default()
                    ));
                }
            };
            if !rec.active {
                return Self::err_text(&format!(
                    "agent '{}' is not active (superseded or deactivated)",
                    p.agent
                ));
            }
            let agent_ref = AgentRef {
                name: rec.name.clone(),
                version: rec.version.parse::<u32>().unwrap_or(1),
            };
            let bro_label = format!("agent:{}@v{}", rec.name, rec.version);
            (manifest, agent_ref, bro_label)
        };
        let runtime_override = match &p.runtime {
            Some(value) => {
                match serde_json::from_value::<orchestration::allocator::RuntimeRequest>(
                    value.clone(),
                ) {
                    Ok(runtime) => Some(runtime),
                    Err(e) => {
                        return Self::err_text(&format!(
                            "error.bad_input(code=invalid_runtime): runtime must be a RuntimeRequest object: {e}"
                        ));
                    }
                }
            }
            None => None,
        };

        if let Err(err) = Self::validate_operator_authority_inputs(&manifest, &p.args) {
            return Self::err_text(&err);
        }

        // Adapter path
        if let Some(ref adapter_name) = manifest.dispatch_adapter {
            if runtime_override.is_some() {
                return Self::err_text(
                    "error.unsupported(code=runtime_with_dispatch_adapter): runtime overrides require the standard bro dispatch path",
                );
            }
            let adapter = {
                let adapter_registry = self.state.agent_adapter_registry.read();
                match adapter_registry.get(adapter_name) {
                    Some(a) => a,
                    None => {
                        return Self::err_text(&format!(
                            "error.bad_input(code=adapter_unavailable): adapter '{}' not registered",
                            adapter_name
                        ));
                    }
                }
            };
            let ctx = DispatchContext {
                project_dir: p.project_dir.clone(),
                ambient: p
                    .ambient
                    .as_ref()
                    .and_then(|v| serde_json::to_string(v).ok()),
                bro_label_prefix: Some(bro_label),
                caller_provider: p.caller_provider.clone(),
                caller_session_id: p.caller_session_id.clone(),
            };
            match adapter.dispatch(&manifest, p.args, ctx).await {
                Ok(result) => {
                    let task_id = result.session.task_id.clone();
                    return Self::ok_json(&serde_json::json!({
                        "session": result.session,
                        "task_id": task_id,
                        "resolved_brofile": result.resolved_brofile,
                        "merged_filters": result.merged_filters,
                        "degraded": result.degraded,
                    }));
                }
                Err(e) => return Self::err_text(&format!("{e}")),
            }
        }

        // Direct path
        let (
            provider,
            lens,
            brofile_name,
            base_allow,
            base_disallow,
            exec_opts,
            env_overrides,
            runtime,
            coerce_workspace,
            brofile_context,
        ) = if let Some(ref br) = manifest.brofile_ref {
            let bf = match orchestration::brofile::resolve_brofile(
                br,
                &self.state.store_dir,
                p.project_dir.as_deref(),
            ) {
                Some(b) => b,
                None => {
                    return Self::err_text(&format!("brofile_ref '{}' not found", br));
                }
            };
            let (ba, bd) = match &bf.filters {
                Some(f) => (f.allow.clone(), f.disallow.clone()),
                None => (Vec::new(), Vec::new()),
            };
            if let Err(err) =
                orchestration::brofile::enforce_provider_defaults(bf.provider, bf.context.as_ref())
            {
                return Self::err_text(&err);
            }
            let env = orchestration::brofile::resolve_provider_env(
                bf.provider,
                bf.account.as_deref(),
                bf.model.as_deref(),
                &self.state.store_dir,
                bf.context.as_ref(),
            );
            let agent_output_schema = manifest
                .outputs
                .as_ref()
                .and_then(|o| o.schema.as_ref())
                .map(|s| s.to_string());
            let opts = if bf.model.is_some()
                || bf.effort.is_some()
                || bf.code_mode.is_some()
                || agent_output_schema.is_some()
            {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                    provider_defaults: None,
                    code_mode: bf.code_mode,
                    // Deliver the agent's declared output schema so a structured
                    // -output agent gets the harness `final_result` terminal tool.
                    output_schema: agent_output_schema,
                })
            } else {
                None
            };
            let opts = orchestration::providers::exec_opts_with_provider_defaults(
                opts,
                bf.context.as_ref(),
            );
            (
                bf.provider,
                bf.lens,
                Some(br.clone()),
                ba,
                bd,
                opts,
                env,
                bf.runtime,
                bf.coerce_workspace.unwrap_or(false),
                bf.context,
            )
        } else if let Some(ref inline) = manifest.brofile_inline {
            let prov_str = inline
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("claude");
            // Parse via serde (not strum FromStr) so the provider taxonomy's
            // aliases are honored — notably `claude` → Glm, which is also the
            // legacy default here. strum's FromStr ignores `#[serde(alias)]`,
            // so the bare `claude` default would otherwise fail closed.
            let provider = match serde_json::from_value::<orchestration::providers::Provider>(
                serde_json::Value::String(prov_str.to_string()),
            ) {
                Ok(p) => p,
                Err(_) => {
                    return Self::err_text(&format!(
                        "error.bad_input(code=unknown_provider): unknown provider in inline brofile: {prov_str}"
                    ));
                }
            };
            // Inline brofiles don't carry context-assembly policy in
            // v1 — the field is only honored on disk-resolved brofiles.
            // Reject rather than silently drop, otherwise an inline
            // strict_suppress declaration would launch without honoring
            // the suppression intent.
            if inline.get("context").is_some() {
                return Self::err_text(
                    "error.bad_input(code=inline_context_unsupported): inline brofile may not declare `context`; use a saved brofile",
                );
            }
            let (ba, bd) = Self::extract_inline_filters(inline);
            let env = orchestration::brofile::resolve_provider_env(
                provider,
                None,
                inline.get("model").and_then(|v| v.as_str()),
                &self.state.store_dir,
                None,
            );
            let opts = if inline.get("model").is_some() || inline.get("effort").is_some() {
                Some(ExecOpts {
                    model: inline
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    effort: inline
                        .get("effort")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    provider_defaults: None,
                    // Inline brofiles don't carry code_mode in v1 (parallels
                    // the inline-context rejection above).
                    code_mode: None,
                    // Inline brofiles don't carry an output schema in v1.
                    output_schema: None,
                })
            } else {
                None
            };
            let lens = inline
                .get("lens")
                .and_then(|v| v.as_str())
                .map(String::from);
            let runtime = inline
                .get("runtime")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            (
                provider,
                lens,
                None,
                ba,
                bd,
                opts,
                env,
                runtime,
                inline
                    .get("coerce_workspace")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                None,
            )
        } else {
            return Self::err_text("manifest has neither brofile_ref nor brofile_inline");
        };

        let merged = MergedFilters::merge(
            &base_allow,
            &base_disallow,
            manifest.filter_overlay.as_ref(),
        );

        if let Some(ref inputs) = manifest.inputs {
            if let Some(ref schema) = inputs.schema {
                let compiled = match jsonschema::JSONSchema::options()
                    .with_draft(jsonschema::Draft::Draft202012)
                    .compile(schema)
                {
                    Ok(c) => c,
                    Err(e) => {
                        return Self::err_text(&format!(
                            "error.internal(code=invalid_schema): manifest schema failed to compile: {e}"
                        ));
                    }
                };
                let args_to_validate = if p.args.is_null() {
                    serde_json::json!({})
                } else {
                    p.args.clone()
                };
                let result = compiled.validate(&args_to_validate);
                if let Err(errors) = result {
                    let msgs: Vec<String> = errors.map(|e| e.to_string()).collect();
                    return Self::err_text(&format!(
                        "error.bad_input(code=schema_validation_failed): {}",
                        msgs.join("; ")
                    ));
                }
            }
        }

        let prompt = match &manifest.inputs {
            Some(spec) => match &spec.prompt_template {
                Some(tmpl) => Self::expand_template(tmpl, &p.args),
                None => {
                    if p.args.is_null() {
                        String::new()
                    } else {
                        serde_json::to_string_pretty(&p.args).unwrap_or_default()
                    }
                }
            },
            None => {
                if p.args.is_null() {
                    String::new()
                } else {
                    serde_json::to_string_pretty(&p.args).unwrap_or_default()
                }
            }
        };

        let cwd = p.project_dir.clone();
        let brofile_filters = orchestration::mcp::McpFilters {
            allow: merged.allow.clone(),
            disallow: merged.disallow.clone(),
        };
        let runtime =
            orchestration::allocator::merge_runtime_request(runtime, manifest.runtime.clone());
        let runtime = orchestration::allocator::merge_runtime_request(runtime, runtime_override);
        let runtime = if manifest
            .outputs
            .as_ref()
            .and_then(|outputs| outputs.schema.as_ref())
            .is_some()
        {
            orchestration::allocator::with_derived_capability(
                runtime,
                orchestration::providers::Capability::StructuredOutput,
            )
        } else {
            runtime
        };
        // If the request is inert (synthesized purely from a derived
        // StructuredOutput capability or the durable flag, with no
        // tier/pool/pin), honor the brofile's declared provider rather than
        // letting the allocator free-select across Provider::ALL and
        // silently override it.
        let runtime = orchestration::allocator::pin_static_provider_if_inert(
            runtime,
            provider,
            exec_opts.as_ref().and_then(|o| o.model.clone()),
            exec_opts.as_ref().and_then(|o| o.effort.clone()),
        );
        // Preserve the resolved code-mode + output schema across the allocator's
        // exec_opts rebuild inside dispatch_fresh_bro_task.
        let agent_code_mode = exec_opts.as_ref().and_then(|o| o.code_mode);
        let agent_output_schema = exec_opts.as_ref().and_then(|o| o.output_schema.clone());
        let dispatched =
            match self.dispatch_fresh_bro_task(crate::tools::dispatch::FreshDispatchRequest {
                prompt,
                provider,
                lens,
                exec_opts,
                env_overrides,
                cwd,
                brofile_filters: Some(brofile_filters),
                coerce_workspace,
                allow_recursion: false,
                allow_tools: None,
                disallow_tools: None,
                tool_placement: None,
                allocation_request: runtime,
                project_dir_for_lease: p.project_dir.clone(),
                ambient_bro_name: p.bro.clone(),
                spawn_bro_label: Some(bro_label.clone()),
                spawn_agent_label: Some(bro_label.clone()),
                record_to_bro: p.bro.clone(),
                brofile_context,
                code_mode: agent_code_mode,
                output_schema: agent_output_schema,
            }) {
                Ok(result) => result,
                Err(e) => return Self::err_text(&e),
            };
        let inner = dispatched.task.inner.lock();
        let task_id = inner.id.clone();
        let session_id = inner.session_id.clone();
        let selected_provider = inner.provider;
        drop(inner);

        let agent_session = AgentSession {
            session_id: session_id.clone(),
            provider: selected_provider.as_str().to_string(),
            project_dir: p.project_dir.clone(),
            agent: agent_ref,
            task_id: Some(task_id.clone()),
        };

        Self::ok_json(&serde_json::json!({
            "session": agent_session,
            "task_id": task_id,
            "resolved_brofile": brofile_name,
            "merged_filters": merged,
            "agentLabel": bro_label,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::artifacts::ArtifactInstallParams;
    use crate::server::install_artifact_value;
    use crate::server::state::SharedState;
    use crate::{embed_queue, entity_ref, vectors};

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn seed_test_agent(
        catalog: &artifacts::ArtifactCatalog,
        file_name: &str,
        name: &str,
        version: u64,
        cost_class: Option<&str>,
    ) {
        let mut manifest = serde_json::json!({
            "description": format!("Test agent {name}."),
            "when_to_use": ["when testing"],
            "brofile_inline": {"provider": "claude"},
        });
        if let Some(cc) = cost_class {
            manifest["cost_class"] = serde_json::json!(cc);
        }
        catalog
            .install_value(
                artifacts::ArtifactKind::Agent,
                file_name.into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": name,
                    "version": version,
                    "manifest": manifest,
                }),
                None,
                None,
                None,
            )
            .unwrap();
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn bro_agent_list_output_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        seed_test_agent(
            &server.state.artifacts.read(),
            "reviewer.json",
            "reviewer",
            1,
            Some("expensive"),
        );
        seed_test_agent(
            &server.state.artifacts.read(),
            "writer.json",
            "writer",
            2,
            Some("cheap"),
        );

        let result = server.bro_agent_list(Parameters(AgentListParams {
            include_superseded: None,
            cost_class: None,
            provenance_kind: None,
            limit: None,
        }));
        assert_ne!(result.is_error, Some(true));

        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let agents = body["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);

        let reviewer = agents.iter().find(|a| a["name"] == "reviewer").unwrap();
        assert_eq!(reviewer["version"], "1");
        assert_eq!(reviewer["active"], true);
        assert_eq!(reviewer["cost_class"], "expensive");
        assert_eq!(reviewer["embedding_pending"], true);

        let writer = agents.iter().find(|a| a["name"] == "writer").unwrap();
        assert_eq!(writer["version"], "2");
        assert_eq!(writer["cost_class"], "cheap");
    }

    #[test]
    fn bro_agent_list_invalid_cost_class_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let result = server.bro_agent_list(Parameters(AgentListParams {
            include_superseded: None,
            cost_class: Some("notavalidclass".into()),
            provenance_kind: None,
            limit: None,
        }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("unknown cost_class"), "got: {text}");
    }

    #[test]
    fn bro_agent_get_found() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        seed_test_agent(
            &server.state.artifacts.read(),
            "reviewer.json",
            "reviewer",
            3,
            None,
        );

        let result = server.bro_agent_get(Parameters(AgentGetParams {
            name: "reviewer".into(),
        }));
        assert_ne!(result.is_error, Some(true));

        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["name"], "reviewer");
        assert_eq!(body["version"], "3");
        assert_eq!(body["active"], true);
        assert!(body["manifest"].is_object());
    }

    #[test]
    fn bro_agent_get_missing_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let result = server.bro_agent_get(Parameters(AgentGetParams {
            name: "nonexistent".into(),
        }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("agent not found"), "got: {text}");
    }

    #[test]
    fn bro_agent_get_pinned_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        seed_test_agent(
            &server.state.artifacts.read(),
            "reviewer.json",
            "reviewer",
            5,
            None,
        );

        let result = server.bro_agent_get(Parameters(AgentGetParams {
            name: "reviewer@v5".into(),
        }));
        assert_ne!(result.is_error, Some(true));

        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["version"], "5");
    }

    #[test]
    fn bro_agent_get_invalid_ref_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let result = server.bro_agent_get(Parameters(AgentGetParams { name: "@v2".into() }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("requires a name"), "got: {text}");
    }

    fn seed_test_agent_with_provenance(
        catalog: &artifacts::ArtifactCatalog,
        file_name: &str,
        name: &str,
        version: u64,
        provenance: serde_json::Value,
    ) {
        let manifest = serde_json::json!({
            "description": format!("Agent {name} with provenance."),
            "when_to_use": ["when testing"],
            "brofile_inline": {"provider": "claude"},
            "provenance": provenance,
        });
        catalog
            .install_value(
                artifacts::ArtifactKind::Agent,
                file_name.into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": name,
                    "version": version,
                    "manifest": manifest,
                }),
                None,
                None,
                None,
            )
            .unwrap();
    }

    #[test]
    fn bro_agent_list_limit_caps_output() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        seed_test_agent(cat, "a1.json", "alpha", 1, None);
        seed_test_agent(cat, "a2.json", "beta", 1, None);
        seed_test_agent(cat, "a3.json", "gamma", 1, None);

        let result = server.bro_agent_list(Parameters(AgentListParams {
            include_superseded: None,
            cost_class: None,
            provenance_kind: None,
            limit: Some(2),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["agents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn bro_agent_list_provenance_kind_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        seed_test_agent(cat, "hand.json", "handmade", 1, None);
        seed_test_agent_with_provenance(
            cat,
            "distilled.json",
            "distilled",
            1,
            serde_json::json!({"kind": "distilled", "distilled_by": "badgey-01", "evidence_session_ids": [], "created_from_threads": [], "accept_count": 5, "reject_count": 0}),
        );

        let result = server.bro_agent_list(Parameters(AgentListParams {
            include_superseded: None,
            cost_class: None,
            provenance_kind: Some("distilled".into()),
            limit: None,
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let agents = body["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["name"], "distilled");
    }

    #[test]
    fn bro_agent_get_pinned_version_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        seed_test_agent(
            &server.state.artifacts.read(),
            "reviewer.json",
            "reviewer",
            5,
            None,
        );

        let result = server.bro_agent_get(Parameters(AgentGetParams {
            name: "reviewer@v4".into(),
        }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("agent not found"), "got: {text}");
    }

    #[test]
    fn bro_agent_list_include_superseded() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        seed_test_agent(cat, "old-agent.json", "old-agent", 1, None);
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "new-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "new-agent",
                "version": 1,
                "supersedes": "old-agent",
                "manifest": {
                    "description": "Replacement for old-agent.",
                    "when_to_use": ["when testing"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            Some("old-agent".into()),
        )
        .unwrap();

        let default_result = server.bro_agent_list(Parameters(AgentListParams {
            include_superseded: None,
            cost_class: None,
            provenance_kind: None,
            limit: None,
        }));
        assert_ne!(default_result.is_error, Some(true));
        let default_body: serde_json::Value =
            serde_json::from_str(&extract_text(&default_result)).unwrap();
        let default_agents = default_body["agents"].as_array().unwrap();
        let default_names: Vec<&str> = default_agents
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert!(
            !default_names.contains(&"old-agent"),
            "superseded agent should be excluded by default: {default_names:?}"
        );
        assert!(
            default_names.contains(&"new-agent"),
            "active agent should be included: {default_names:?}"
        );

        let with_superseded = server.bro_agent_list(Parameters(AgentListParams {
            include_superseded: Some(true),
            cost_class: None,
            provenance_kind: None,
            limit: None,
        }));
        assert_ne!(with_superseded.is_error, Some(true));
        let all_body: serde_json::Value =
            serde_json::from_str(&extract_text(&with_superseded)).unwrap();
        let all_agents = all_body["agents"].as_array().unwrap();
        let all_names: Vec<&str> = all_agents
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert!(
            all_names.contains(&"old-agent"),
            "superseded agent should appear with include_superseded=true: {all_names:?}"
        );
        let old = all_agents
            .iter()
            .find(|a| a["name"] == "old-agent")
            .unwrap();
        assert_eq!(old["active"], false);
    }
    #[test]
    fn bro_agent_describe_inline_brofile() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        seed_test_agent(
            &server.state.artifacts.read(),
            "reviewer.json",
            "reviewer",
            1,
            Some("normal"),
        );

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "reviewer".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["name"], "reviewer");
        assert_eq!(body["version"], "1");
        assert_eq!(body["active"], true);
        assert_eq!(body["brofile_kind"], "inline");
        assert_eq!(body["brofile_provider"], "claude");
        assert_eq!(body["embedding_status"], "pending");
        assert!(body["manifest"].is_object());
        assert!(body["merged_filters"].is_object());
        assert!(body["brofile"].is_object());
        assert_eq!(body["brofile"]["provider"], "claude");
        assert!(body["install_warnings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn bro_agent_describe_inline_brofile_filters_in_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        let manifest = serde_json::json!({
            "description": "Agent with inline brofile filters.",
            "when_to_use": ["when testing"],
            "brofile_inline": {
                "provider": "claude",
                "filters": {
                    "allow": ["mcp__blackbox__bbox_search"],
                    "disallow": ["mcp__blackbox__bro_exec"]
                }
            },
        });
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "inline-filtered.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "inline-filtered",
                "version": 1,
                "manifest": manifest,
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "inline-filtered".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let allow = body["merged_filters"]["allow"].as_array().unwrap();
        let disallow = body["merged_filters"]["disallow"].as_array().unwrap();
        assert!(
            allow
                .iter()
                .any(|p| p.as_str() == Some("mcp__blackbox__bbox_search")),
            "inline allow should appear in merged: {allow:?}"
        );
        assert!(
            disallow
                .iter()
                .any(|p| p.as_str() == Some("mcp__blackbox__bro_exec")),
            "inline disallow should appear in merged: {disallow:?}"
        );
    }

    #[test]
    fn bro_agent_describe_missing_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "nonexistent".into(),
        }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("agent not found"), "got: {text}");
    }

    #[test]
    fn bro_agent_describe_deny_wins_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        let manifest = serde_json::json!({
            "description": "Agent where overlay allows what brofile denies.",
            "when_to_use": ["when testing"],
            "brofile_inline": {
                "provider": "claude",
                "filters": {
                    "allow": ["mcp__blackbox__bbox_search"],
                    "disallow": ["mcp__blackbox__bro_exec", "mcp__blackbox__bbox_cite"]
                }
            },
            "filter_overlay": {
                "allow": ["mcp__blackbox__bro_exec", "mcp__blackbox__bbox_cite"],
                "disallow": []
            },
        });
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "conflict.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "conflict",
                "version": 1,
                "manifest": manifest,
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "conflict".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let allow = body["merged_filters"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        let disallow = body["merged_filters"]["disallow"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert!(
            !allow.contains(&"mcp__blackbox__bro_exec"),
            "overlay allow should be stripped by deny-wins: {allow:?}"
        );
        assert!(
            !allow.contains(&"mcp__blackbox__bbox_cite"),
            "overlay allow for cite should be stripped by deny-wins: {allow:?}"
        );
        assert!(
            allow.contains(&"mcp__blackbox__bbox_search"),
            "non-conflicting allow should survive: {allow:?}"
        );
        assert!(
            disallow.contains(&"mcp__blackbox__bro_exec"),
            "disallow should win: {disallow:?}"
        );
        assert!(
            disallow.contains(&"mcp__blackbox__bbox_cite"),
            "disallow should win for cite too: {disallow:?}"
        );
    }

    #[tokio::test]
    async fn agent_install_records_filter_conflict_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_artifact_value(
            &server.state,
            artifacts::ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Agent,
                source: "inline".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            serde_json::json!({
                "kind": "agent",
                "name": "warning-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent where overlay conflicts are recorded.",
                    "when_to_use": ["when testing filter warnings"],
                    "brofile_inline": {
                        "provider": "claude",
                        "filters": {
                            "allow": ["Read"],
                            "disallow": ["Bash"]
                        }
                    },
                    "filter_overlay": {
                        "allow": ["Bash"],
                        "disallow": ["Read"]
                    }
                }
            }),
        )
        .await
        .unwrap();

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "warning-agent".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let warnings = body["install_warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 2, "warnings: {warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().is_some_and(|s| s.contains("Bash"))),
            "allow/disallow conflict warning missing: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().is_some_and(|s| s.contains("Read"))),
            "disallow/allow conflict warning missing: {warnings:?}"
        );
    }

    #[test]
    fn bro_agent_describe_brofile_ref_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        use orchestration::brofile::Brofile;
        use orchestration::mcp::McpFilters;
        use orchestration::providers::Provider;
        let bf = Brofile {
            name: "auditor".into(),
            provider: Provider::Glm,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: Some(McpFilters {
                allow: vec![
                    "mcp__blackbox__bbox_search".into(),
                    "mcp__blackbox__bbox_cite".into(),
                ],
                disallow: vec!["mcp__blackbox__bro_exec".into()],
            }),
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
        };
        let _ = orchestration::brofile::save_brofile(&bf, "global", &server.state.store_dir, None);
        let manifest = serde_json::json!({
            "description": "Agent referencing a saved brofile.",
            "when_to_use": ["when auditing"],
            "brofile_ref": "auditor",
        });
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "auditor-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "auditor-agent",
                "version": 1,
                "manifest": manifest,
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "auditor-agent".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["brofile_kind"], "ref");
        assert_eq!(body["brofile_name"], "auditor");
        // The saved brofile is Provider::Glm; serde serializes it lowercase.
        // (`claude` is only a deserialize alias for Glm, not a serialized form.)
        assert_eq!(body["brofile_provider"], "glm");
        assert!(body["brofile"].is_object());
        assert_eq!(body["brofile"]["name"], "auditor");
        assert_eq!(body["brofile"]["provider"], "glm");
        let allow = body["merged_filters"]["allow"].as_array().unwrap();
        let disallow = body["merged_filters"]["disallow"].as_array().unwrap();
        assert!(
            allow
                .iter()
                .any(|p| p.as_str() == Some("mcp__blackbox__bbox_search")),
            "brofile allow in merged: {allow:?}"
        );
        assert!(
            disallow
                .iter()
                .any(|p| p.as_str() == Some("mcp__blackbox__bro_exec")),
            "brofile disallow in merged: {disallow:?}"
        );
    }

    #[tokio::test]
    async fn bro_agent_describe_flags_stale_brofile_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_artifact_value(
            &server.state,
            artifacts::ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: "inline".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            serde_json::json!({
                "name": "review-persona",
                "version": 1,
                "provider": "claude",
                "lens": "Review code."
            }),
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            artifacts::ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Agent,
                source: "inline".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            serde_json::json!({
                "kind": "agent",
                "name": "stale-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with a brofile ref that will become stale.",
                    "when_to_use": ["when testing stale refs"],
                    "brofile_ref": "review-persona"
                }
            }),
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            artifacts::ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: "inline".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            serde_json::json!({
                "name": "review-persona-v2",
                "version": 2,
                "supersedes": "review-persona",
                "provider": "claude",
                "lens": "Review code better."
            }),
        )
        .await
        .unwrap();

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "stale-agent".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["degraded"]["manifest_stale"].as_bool(), Some(true));
        assert!(
            body["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning
                    .as_str()
                    .is_some_and(|text| text.contains("review-persona-v2"))),
            "expected stale warning: {body}"
        );
    }

    #[test]
    fn bro_agent_describe_degraded_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "broken.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "broken",
                "version": 1,
                "manifest": 42,
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
            agent: "broken".into(),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["name"], "broken");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("manifest parse failed")
        );
    }
    #[test]
    fn bro_agent_search_result_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "reviewer.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "code-reviewer",
                "version": 1,
                "manifest": {
                    "description": "Reviews pull requests for code quality and security.",
                    "when_to_use": ["when you need a PR reviewed"],
                    "anti_patterns": ["reviewing your own code"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "review pull request security".into(),
            limit: None,
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: None,
            query_vector: None,
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["search_mode"], "keyword");
        assert!(body["results"].is_array());
        assert!(body["total_matched"].as_u64().unwrap() > 0);
        assert!(body["active_agents"].as_u64().unwrap() > 0);
        assert!(body["degraded"]["embedding_pending"].as_bool().unwrap());
        assert!(body["vector_status"]["coverage_ratio"].is_number());
        let first = &body["results"][0];
        assert_eq!(first["name"], "code-reviewer");
        assert!(first["score"].as_f64().unwrap() > 0.0);
        assert!(first["description"].is_string());
        assert!(first["when_to_use"].is_array());
        assert!(first["anti_patterns"].is_array());
    }

    #[test]
    fn bro_agent_search_uses_agent_manifest_vectors_when_available() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let vector_store =
            std::sync::Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
        let _guard = vectors::install_test_global(vector_store.clone());
        let route = embed::EmbeddingRouter::default()
            .route(embed::Bucket::AgentManifest, None)
            .unwrap()
            .vector_route_id();
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "semantic-reviewer.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "semantic-reviewer",
                "version": 1,
                "manifest": {
                    "description": "Reviews change sets with semantic ranking.",
                    "when_to_use": ["when a vector query should find this agent"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        let agent = orchestration::agents::types::AgentRef {
            name: "semantic-reviewer".into(),
            version: 1,
        };
        vector_store
            .upsert(
                &route,
                &embed_queue::agent_component_entity_id(
                    &agent,
                    embed_queue::AgentManifestComponent::Primary,
                ),
                "h1",
                vec![1.0, 0.0, 0.0],
            )
            .unwrap();

        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "orthogonal words".into(),
            limit: Some(5),
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: Some(true),
            query_vector: Some(vec![1.0, 0.0, 0.0]),
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["search_mode"], "hybrid");
        let first = &body["results"][0];
        assert_eq!(first["name"], "semantic-reviewer");
        assert!(first["sources"]["vector_primary"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn agent_install_stamps_embeddings_and_distilled_edges() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let artifact = serde_json::json!({
            "kind": "agent",
            "name": "distilled-reviewer",
            "version": 1,
            "manifest": {
                "description": "Reviews recurring code patterns.",
                "when_to_use": ["when recurring review work appears"],
                "anti_patterns": ["one-off typo fixes"],
                "brofile_inline": {"provider": "claude"},
                "provenance": {
                    "kind": "distilled",
                    "distilled_by": "badgey-01",
                    "evidence_session_ids": ["session:claude:sess-1"],
                    "created_from_threads": ["thread:thread-abc"],
                    "accept_count": 1,
                    "reject_count": 0
                }
            }
        });
        rt.block_on(install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Agent,
                source: "distilled-reviewer.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            artifact,
        ))
        .unwrap();

        let stored = server
            .state
            .artifacts
            .read()
            .load_artifact_value(artifacts::ArtifactKind::Agent, "distilled-reviewer")
            .unwrap()
            .unwrap();
        let embedding = &stored["manifest"]["embedding"];
        assert_eq!(
            embedding["components"]["primary"],
            "agent_embed:distilled-reviewer:v1:primary"
        );
        assert_eq!(
            embedding["components"]["when_to_use"],
            "agent_embed:distilled-reviewer:v1:when_to_use"
        );
        assert_eq!(
            embedding["components"]["anti_patterns"],
            "agent_embed:distilled-reviewer:v1:anti_patterns"
        );

        let agent_ref = entity_ref::EntityRef::Agent {
            name: "distilled-reviewer".into(),
            version: 1,
        };
        let edge_index = server.state.edge_index.read();
        let edges = edge_index.forward_edges_filtered(&agent_ref, &["DERIVED_FROM"]);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().any(|edge| {
            edge.target
                == entity_ref::EntityRef::Session {
                    provider: "claude".into(),
                    session_id: "sess-1".into(),
                }
        }));
        assert!(edges.iter().any(|edge| {
            edge.target
                == entity_ref::EntityRef::Thread {
                    thread_id: "thread-abc".into(),
                }
        }));
    }

    #[test]
    fn bro_agent_search_limit_caps_output() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        for i in 0..5 {
            cat.install_value(
                artifacts::ArtifactKind::Agent,
                format!("agent{i}.json"),
                &serde_json::json!({
                    "kind": "agent",
                    "name": format!("search-agent-{i}"),
                    "version": 1,
                    "manifest": {
                        "description": format!("Agent {i} for testing search functionality."),
                        "when_to_use": ["when testing search"],
                        "brofile_inline": {"provider": "claude"},
                    },
                }),
                None,
                None,
                None,
            )
            .unwrap();
        }

        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "testing search".into(),
            limit: Some(2),
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: None,
            query_vector: None,
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert!(body["results"].as_array().unwrap().len() <= 2);
    }

    #[test]
    fn bro_agent_search_empty_query_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "  ".into(),
            limit: None,
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: None,
            query_vector: None,
        }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("query is required"), "got: {text}");
    }

    #[test]
    fn bro_agent_search_default_excludes_anti_pattern_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "ap-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "deploy-bot",
                "version": 1,
                "manifest": {
                    "description": "Deploys code to production.",
                    "when_to_use": ["when deploying to production"],
                    "anti_patterns": ["deploying untested code"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "deploy untested code".into(),
            limit: None,
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: None,
            query_vector: None,
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let results = body["results"].as_array().unwrap();
        assert!(
            results.is_empty(),
            "anti-pattern match should be excluded by default: {results:?}"
        );
    }

    #[test]
    fn bro_agent_search_include_anti_pattern_returns_matched() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "ap-agent2.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "deploy-bot-2",
                "version": 1,
                "manifest": {
                    "description": "Deploys code to production safely.",
                    "when_to_use": ["when deploying to production"],
                    "anti_patterns": ["deploying untested code"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "deploy untested code".into(),
            limit: None,
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: Some(false),
            include_vectors: None,
            query_vector: None,
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let results = body["results"].as_array().unwrap();
        assert!(
            !results.is_empty(),
            "should return result when exclude=false"
        );
        let matched_ap = results[0]["matched_anti_patterns"].as_array().unwrap();
        assert!(
            matched_ap
                .iter()
                .any(|v| v.as_str().unwrap().contains("untested")),
            "matched_anti_patterns should include the matching anti-pattern: {matched_ap:?}"
        );
    }

    #[test]
    fn bro_agent_search_inactive_agents_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "active.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "active-agent",
                "version": 1,
                "manifest": {
                    "description": "An active search test agent.",
                    "when_to_use": ["when testing"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "retired-v1.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "retired-agent",
                "version": 1,
                "manifest": {
                    "description": "A retired search test agent.",
                    "when_to_use": ["when testing"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "replacement-v2.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "retired-agent",
                "version": 2,
                "manifest": {
                    "description": "An active replacement search test agent.",
                    "when_to_use": ["when testing"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            Some("retired-agent".to_string()),
        )
        .unwrap();

        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "search test agent".into(),
            limit: None,
            cost_class: None,
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: None,
            query_vector: None,
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let results = body["results"].as_array().unwrap();
        let retired_entries: Vec<_> = results
            .iter()
            .filter(|r| r["name"].as_str() == Some("retired-agent"))
            .collect();
        assert_eq!(
            retired_entries.len(),
            1,
            "only v2 of retired-agent should appear"
        );
        assert_eq!(retired_entries[0]["version"], "2");
        assert!(
            results
                .iter()
                .any(|r| r["name"].as_str() == Some("active-agent")),
            "active-agent should appear: {results:?}"
        );
    }

    #[test]
    fn bro_agent_search_invalid_cost_class_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let result = server.bro_agent_search(Parameters(AgentSearchParams {
            query: "test".into(),
            limit: None,
            cost_class: Some("invalid".into()),
            provenance_kind: None,
            exclude_anti_pattern_matches: None,
            include_vectors: None,
            query_vector: None,
        }));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("invalid cost_class"), "got: {text}");
    }
    #[test]
    fn bro_agent_dispatch_agent_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "nonexistent".into(),
            args: serde_json::Value::Null,
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("agent not found"), "got: {text}");
    }

    #[test]
    fn bro_agent_dispatch_inactive_agent_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "v1.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "inactive-dispatch",
                "version": 1,
                "manifest": {
                    "description": "Test agent.",
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "v2.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "inactive-dispatch",
                "version": 2,
                "manifest": {
                    "description": "Replacement.",
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            Some("inactive-dispatch".to_string()),
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "inactive-dispatch@v1".into(),
            args: serde_json::Value::Null,
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("not active") || text.contains("agent not found"),
            "got: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_adapter_unavailable_hard_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "badgey.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "badgey-agent",
                "version": 1,
                "manifest": {
                    "description": "Badgey agent.",
                    "dispatch_adapter": "badgey",
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "badgey-agent".into(),
            args: serde_json::Value::Null,
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("adapter_unavailable"),
            "should hard-fail with adapter_unavailable: {text}"
        );
    }

    #[tokio::test]
    async fn bro_agent_dispatch_noop_adapter_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        server
            .state
            .artifacts
            .read()
            .install_value(
                artifacts::ArtifactKind::Agent,
                "noop-dispatch.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "noop-agent",
                    "version": 1,
                    "manifest": {
                        "description": "Noop test agent.",
                        "dispatch_adapter": "noop",
                        "brofile_inline": {"provider": "claude"},
                    },
                }),
                None,
                None,
                None,
            )
            .unwrap();

        struct NoopAdapter;
        impl orchestration::agents::adapter::AgentDispatchAdapter for NoopAdapter {
            fn name(&self) -> &'static str {
                "noop"
            }
            fn dispatch(
                &self,
                _manifest: &orchestration::agents::types::AgentManifest,
                _args: serde_json::Value,
                ctx: orchestration::agents::adapter::DispatchContext,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                orchestration::agents::adapter::AgentDispatchResult,
                                orchestration::agents::adapter::AgentDispatchError,
                            >,
                        > + Send
                        + '_,
                >,
            > {
                let caller_provider = ctx.caller_provider.clone().unwrap_or_default();
                let caller_session_id = ctx.caller_session_id.clone().unwrap_or_default();
                Box::pin(async move {
                    Ok(orchestration::agents::adapter::AgentDispatchResult {
                        session: orchestration::agents::types::AgentSession {
                            session_id: format!("{caller_provider}/{caller_session_id}"),
                            provider: "test".into(),
                            project_dir: None,
                            agent: orchestration::agents::types::AgentRef {
                                name: "noop-agent".into(),
                                version: 1,
                            },
                            task_id: Some("test-task-456".into()),
                        },
                        resolved_brofile: None,
                        merged_filters: orchestration::agents::types::MergedFilters::default(),
                        degraded: None,
                    })
                })
            }
        }
        server
            .state
            .agent_adapter_registry
            .write()
            .register(std::sync::Arc::new(NoopAdapter));

        let result = server
            .bro_agent_dispatch(Parameters(AgentDispatchParams {
                agent: "noop-agent".into(),
                args: serde_json::json!({"prompt": "hello"}),
                project_dir: None,
                bro: None,
                ambient: None,
                caller_provider: Some("claude".into()),
                caller_session_id: Some("caller-session-789".into()),
                runtime: None,
            }))
            .await;
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["session"]["session_id"], "claude/caller-session-789");
        assert_eq!(body["task_id"], "test-task-456");
    }

    #[tokio::test]
    async fn bro_agent_dispatch_handle_shape_reference_agent_noop_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let mut diff_narrator: serde_json::Value = serde_json::from_str(include_str!(
            "../../system-defaults/agents/diff-narrator.json"
        ))
        .unwrap();
        diff_narrator["manifest"]["dispatch_adapter"] = serde_json::json!("noop-ref");
        server
            .state
            .artifacts
            .read()
            .install_value(
                artifacts::ArtifactKind::Agent,
                "diff-narrator-ref.json".into(),
                &diff_narrator,
                None,
                None,
                None,
            )
            .unwrap();

        struct RefNoopAdapter;
        impl orchestration::agents::adapter::AgentDispatchAdapter for RefNoopAdapter {
            fn name(&self) -> &'static str {
                "noop-ref"
            }
            fn dispatch(
                &self,
                manifest: &orchestration::agents::types::AgentManifest,
                _args: serde_json::Value,
                ctx: orchestration::agents::adapter::DispatchContext,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                orchestration::agents::adapter::AgentDispatchResult,
                                orchestration::agents::adapter::AgentDispatchError,
                            >,
                        > + Send,
                >,
            > {
                let description = manifest.description.clone();
                Box::pin(async move {
                    Ok(orchestration::agents::adapter::AgentDispatchResult {
                        session: orchestration::agents::types::AgentSession {
                            session_id: format!("ref-session-{description}"),
                            provider: "stub-provider".into(),
                            project_dir: ctx.project_dir.clone(),
                            agent: orchestration::agents::types::AgentRef {
                                name: "diff-narrator".into(),
                                version: 1,
                            },
                            task_id: Some(format!("ref-task-{description}")),
                        },
                        resolved_brofile: Some("diff-narrator-inline-brofile".into()),
                        merged_filters: orchestration::agents::types::MergedFilters {
                            allow: vec!["mcp__blackbox__bbox_*".into()],
                            disallow: vec![],
                        },
                        degraded: None,
                    })
                })
            }
        }
        server
            .state
            .agent_adapter_registry
            .write()
            .register(std::sync::Arc::new(RefNoopAdapter));

        let result = server
            .bro_agent_dispatch(Parameters(AgentDispatchParams {
                agent: "diff-narrator".into(),
                args: serde_json::json!({"diff": "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new"}),
                project_dir: Some("/tmp/test".into()),
                bro: None,
                ambient: None,
                caller_provider: None,
                caller_session_id: None,
                runtime: None,
            }))
            .await;
        assert!(
            !result.is_error.unwrap_or(false),
            "bro_agent_dispatch should succeed: {}",
            extract_text(&result)
        );
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();

        // Verify handle shape: every field the agent-system.md 5.3 contract requires
        assert!(body["session"].is_object(), "should have session object");
        assert!(
            body["session"]["session_id"].is_string(),
            "session.session_id must be a string"
        );
        assert!(
            !body["session"]["session_id"].as_str().unwrap().is_empty(),
            "session_id must be non-empty"
        );
        assert_eq!(
            body["session"]["provider"], "stub-provider",
            "session.provider should match adapter output"
        );
        assert_eq!(
            body["session"]["project_dir"], "/tmp/test",
            "session.project_dir should echo DispatchContext"
        );
        assert!(
            body["session"]["agent"].is_object(),
            "session.agent must be an object"
        );
        assert_eq!(
            body["session"]["agent"]["name"], "diff-narrator",
            "agent.name should be diff-narrator"
        );
        assert_eq!(
            body["session"]["agent"]["version"], 1,
            "agent.version should be 1"
        );
        assert!(
            body["session"]["task_id"].is_string(),
            "session.task_id must be a string"
        );
        assert!(
            !body["session"]["task_id"].as_str().unwrap().is_empty(),
            "session.task_id must be non-empty"
        );
        assert!(
            body["task_id"].is_string(),
            "top-level task_id must be a string"
        );
        assert!(
            !body["task_id"].as_str().unwrap().is_empty(),
            "top-level task_id must be non-empty"
        );
        assert_eq!(
            body["task_id"], body["session"]["task_id"],
            "top-level task_id should match session.task_id"
        );
        assert!(
            body["resolved_brofile"].is_string(),
            "resolved_brofile should be present"
        );
        assert!(
            body["merged_filters"].is_object(),
            "merged_filters should be an object"
        );
        assert!(
            body["degraded"].is_null(),
            "degraded should be null for successful dispatch"
        );
    }

    #[test]
    fn bro_agent_dispatch_unparseable_manifest_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "broken-dispatch.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "broken-dispatch",
                "version": 1,
                "manifest": 42,
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "broken-dispatch".into(),
            args: serde_json::Value::Null,
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(text.contains("unparseable manifest"), "got: {text}");
    }

    #[test]
    fn bro_agent_dispatch_no_brofile_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "no-brofile.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "no-brofile-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent without brofile.",
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "no-brofile-agent".into(),
            args: serde_json::Value::Null,
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("neither brofile_ref nor brofile_inline"),
            "got: {text}"
        );
    }

    #[test]
    fn expand_template_substitutes_args() {
        let tmpl = "Review {{diff}} for {{focus}} issues.";
        let args = serde_json::json!({"diff": "abc123", "focus": "security"});
        let result = BlackboxServer::expand_template(tmpl, &args);
        assert_eq!(result, "Review abc123 for security issues.");
    }

    #[test]
    fn bro_agent_dispatch_rejects_undeclared_operator_authority_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "ack-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "ack-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with undeclared acknowledge arg.",
                    "brofile_inline": {"provider": "claude"},
                    "inputs": {
                        "schema": {
                            "type": "object",
                            "properties": {
                                "project_dir": {"type": "string"}
                            }
                        },
                        "prompt_template": "Run on {{project_dir}}"
                    },
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "ack-agent".into(),
            args: serde_json::json!({
                "project_dir": "/tmp/x",
                "acknowledge_repr": true
            }),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("operator_authority_flag_not_declared")
                && text.contains("acknowledge_repr"),
            "got: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_rejects_hardcoded_operator_authority_template_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
        artifacts::ArtifactKind::Agent,
        "ack-constant-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "ack-constant-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with hardcoded acknowledge flag.",
                "brofile_inline": {"provider": "claude"},
                "inputs": {
                    "schema": {
                        "type": "object",
                        "properties": {
                            "project_dir": {"type": "string"},
                            "acknowledge_public_api_change": {"type": "boolean"}
                        }
                    },
                    "prompt_template": "bbox_refactor_run(confirm=true, steps=[], toml_entries={\"acknowledge_public_api_change\": true})"
                },
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "ack-constant-agent".into(),
            args: serde_json::json!({"project_dir": "/tmp/x"}),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("operator_authority_flag_constant")
                && text.contains("acknowledge_public_api_change"),
            "got: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_rejects_compact_hardcoded_operator_authority_template_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
        artifacts::ArtifactKind::Agent,
        "ack-compact-constant-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "ack-compact-constant-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with compact hardcoded acknowledge flag.",
                "brofile_inline": {"provider": "claude"},
                "inputs": {
                    "schema": {
                        "type": "object",
                        "properties": {
                            "project_dir": {"type": "string"},
                            "acknowledge_public_api_change": {"type": "boolean"}
                        }
                    },
                    "prompt_template": "bbox_refactor_run(confirm=true, toml_entries={\"acknowledge_public_api_change\":true})"
                },
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "ack-compact-constant-agent".into(),
            args: serde_json::json!({"project_dir": "/tmp/x"}),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("operator_authority_flag_constant")
                && text.contains("acknowledge_public_api_change"),
            "got: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_accepts_declared_operator_authority_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "ack-declared-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "ack-declared-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with declared acknowledge arg.",
                    "dispatch_adapter": "missing-adapter",
                    "brofile_inline": {"provider": "claude"},
                    "inputs": {
                        "schema": {
                            "type": "object",
                            "properties": {
                                "project_dir": {"type": "string"},
                                "acknowledge_repr": {"type": "boolean"}
                            }
                        },
                        "prompt_template": "Run on {{project_dir}} with {{acknowledge_repr}}"
                    },
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "ack-declared-agent".into(),
            args: serde_json::json!({
                "project_dir": "/tmp/x",
                "acknowledge_repr": true
            }),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("adapter_unavailable")
                && !text.contains("operator_authority_flag_not_declared"),
            "got: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_invalid_inline_provider_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "bad-prov.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "bad-provider-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with bad provider.",
                    "brofile_inline": {"provider": "nonexistent_provider_xyz"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "bad-provider-agent".into(),
            args: serde_json::Value::Null,
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("unknown_provider") && text.contains("nonexistent_provider_xyz"),
            "should report unknown provider: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_args_validation_missing_required() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "schema-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "schema-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with input schema.",
                    "brofile_inline": {"provider": "claude"},
                    "inputs": {
                        "schema": {
                            "type": "object",
                            "required": ["diff", "focus"],
                        },
                        "prompt_template": "Review {{diff}} for {{focus}} issues."
                    },
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "schema-agent".into(),
            args: serde_json::json!({"diff": "abc123"}),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("schema_validation_failed") && text.contains("focus"),
            "should report schema validation failure for 'focus': {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_args_type_mismatch_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "typed-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "typed-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with typed schema.",
                    "brofile_inline": {"provider": "claude"},
                    "inputs": {
                        "schema": {
                            "type": "object",
                            "properties": {
                                "diff": {"type": "string"}
                            },
                            "required": ["diff"],
                        },
                        "prompt_template": "Review {{diff}}."
                    },
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "typed-agent".into(),
            args: serde_json::json!({"diff": 123}),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("schema_validation_failed"),
            "should reject wrong type: {text}"
        );
    }

    #[test]
    fn bro_agent_dispatch_schema_202012_prefix_items_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "tuple-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "tuple-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent with 2020-12 prefixItems schema.",
                    "brofile_inline": {"provider": "claude"},
                    "inputs": {
                        "schema": {
                            "type": "object",
                            "properties": {
                                "coords": {
                                    "type": "array",
                                    "prefixItems": [
                                        {"type": "number"},
                                        {"type": "number"}
                                    ]
                                }
                            },
                            "required": ["coords"],
                        },
                        "prompt_template": "Plot {{coords}}."
                    },
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "tuple-agent".into(),
            args: serde_json::json!({"coords": ["not-a-number", 2]}),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_eq!(result.is_error, Some(true));
        let text = extract_text(&result);
        assert!(
            text.contains("schema_validation_failed"),
            "should reject via prefixItems: {text}"
        );
    }
    #[test]
    fn bro_agent_dispatch_bro_label_stamped_on_task() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "labeled.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "labeled-agent",
                "version": 3,
                "manifest": {
                    "description": "Agent whose bro_label should be stamped.",
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "labeled-agent".into(),
            args: serde_json::Value::Null,
            project_dir: Some(tmp.path().to_str().unwrap().to_string()),
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(
            body["agentLabel"].as_str(),
            Some("agent:labeled-agent@v3"),
            "response should carry agentLabel: {body}"
        );
        let task_id = body["task_id"].as_str().unwrap();
        let task = server.state.task_store.read().get(task_id).unwrap();
        let label = task.inner.lock().bro_label.clone();
        assert_eq!(
            label.as_deref(),
            Some("agent:labeled-agent@v3"),
            "bro_label should be stamped: {label:?}"
        );
        let agent_lbl = task.inner.lock().agent_label.clone();
        assert_eq!(
            agent_lbl.as_deref(),
            Some("agent:labeled-agent@v3"),
            "agent_label should be stamped: {agent_lbl:?}"
        );
    }

    #[test]
    fn bro_agent_dispatch_with_bro_preserves_agent_label() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "bro-dispatch.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "bro-dispatch-agent",
                "version": 2,
                "manifest": {
                    "description": "Agent dispatched with bro=.",
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "bro-dispatch-agent".into(),
            args: serde_json::Value::Null,
            project_dir: Some(tmp.path().to_str().unwrap().to_string()),
            bro: Some("some-bro".into()),
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = body["task_id"].as_str().unwrap();
        let task = server.state.task_store.read().get(task_id).unwrap();
        let inner = task.inner.lock();
        let bro_lbl = inner.bro_label.clone();
        assert_eq!(
            bro_lbl.as_deref(),
            Some("some-bro"),
            "bro_label should be the named bro: {bro_lbl:?}"
        );
        let agent_lbl = inner.agent_label.clone();
        assert_eq!(
            agent_lbl.as_deref(),
            Some("agent:bro-dispatch-agent@v2"),
            "agent_label should preserve agent attribution: {agent_lbl:?}"
        );
    }
}
