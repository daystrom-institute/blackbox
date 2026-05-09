use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::agents_tools()
}

#[tool_router(router = agents_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_agent_list",
        description = "List installed agents from the registry. Optional filters for cost_class, provenance_kind, include_superseded, and limit."
    )]
    pub(crate) fn bro_agent_list(&self, Parameters(p): Parameters<AgentListParams>) -> CallToolResult {
        use orchestration::agents::registry::{AgentRegistry, ListFilter};
        use orchestration::agents::types::AgentCostClass;
        let catalog = self.state.artifacts.read();
        let reg = AgentRegistry::new(&catalog);
        let cost_class = match p.cost_class.as_deref() {
            Some(s) => {
                let parsed: AgentCostClass =
                    match serde_json::from_value(serde_json::Value::String(s.to_string())) {
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
    pub(crate) fn bro_agent_get(&self, Parameters(p): Parameters<AgentGetParams>) -> CallToolResult {
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
    pub(crate) fn bro_agent_describe(&self, Parameters(p): Parameters<AgentDescribeParams>) -> CallToolResult {
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
    pub(crate) fn bro_agent_search(&self, Parameters(p): Parameters<AgentSearchParams>) -> CallToolResult {
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

        // Adapter path
        if let Some(ref adapter_name) = manifest.dispatch_adapter {
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
        let (provider, lens, brofile_name, base_allow, base_disallow, exec_opts, env_overrides) =
            if let Some(ref br) = manifest.brofile_ref {
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
                let env = orchestration::brofile::resolve_provider_env(
                    bf.provider,
                    bf.account.as_deref(),
                    bf.model.as_deref(),
                    &self.state.store_dir,
                );
                let opts = if bf.model.is_some() || bf.effort.is_some() {
                    Some(ExecOpts {
                        model: bf.model.clone(),
                        effort: bf.effort.clone(),
                    })
                } else {
                    None
                };
                (bf.provider, bf.lens, Some(br.clone()), ba, bd, opts, env)
            } else if let Some(ref inline) = manifest.brofile_inline {
                let prov_str = inline
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("claude");
                let provider = match prov_str.parse::<orchestration::providers::Provider>() {
                    Ok(p) => p,
                    Err(_) => {
                        return Self::err_text(&format!(
                            "error.bad_input(code=unknown_provider): unknown provider in inline brofile: {prov_str}"
                        ));
                    }
                };
                let (ba, bd) = Self::extract_inline_filters(inline);
                let env = orchestration::brofile::resolve_provider_env(
                    provider,
                    None,
                    inline.get("model").and_then(|v| v.as_str()),
                    &self.state.store_dir,
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
                    })
                } else {
                    None
                };
                let lens = inline
                    .get("lens")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                (provider, lens, None, ba, bd, opts, env)
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

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = if matches!(provider, Provider::Claude) {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let cwd = p.project_dir.clone();

        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                p.bro.as_deref(),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(provider),
        };
        let final_prompt =
            orch::apply_brofile_lens(&orch::apply_ambient(&prompt, &ambient_ctx), lens.as_deref());

        let mut args = provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let brofile_filters = orchestration::mcp::McpFilters {
            allow: merged.allow.clone(),
            disallow: merged.disallow.clone(),
        };
        let extra = combine_dispatch_filters(Some(&brofile_filters), None);
        let dispatch_filters =
            resolve_dispatch_filters(provider, cwd.as_deref(), false, &task_id, extra.as_ref());
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id.clone(),
            provider,
            args,
            session_id.clone(),
            cwd,
            env_overrides,
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some(bro_label.clone()),
            Some(bro_label.clone()),
        );

        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);

        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let agent_session = AgentSession {
            session_id: session_id.clone(),
            provider: provider.as_str().to_string(),
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
