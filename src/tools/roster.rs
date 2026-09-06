use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::notes;
use crate::orchestration;
use crate::orchestration as orch;
use crate::orchestration::providers::dispatch_prelude::*;
use crate::orchestration::providers::{ExecOpts, Provider};
use crate::packets::apply_with as apply_packet_with;
use crate::server::progress::{
    cleanup_policy_file_when_done, extra_filters_from_params, release_resume_lease_when_done,
    resolve_dispatch_filters, try_acquire_resume_lease,
};
use crate::server::state::BlackboxServer;
use crate::tools::bro_params::{
    AdvisorCheckpoint, AdvisorMemberCheckpoint, AdvisorNoteSummary, AdvisorSpecParams,
    BrofileParams, DashboardParams, ProvidersParams, ReportParams, TeamParams,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::roster_tools()
}

/// Per-agent rollup accumulated while scanning the task store for
/// `bro_dashboard`. `dispatch_count` is the always-populated anchor (an agent
/// only enters the map once it has been dispatched at least once); the rest are
/// signal-gated on serialization so idle/still-running agents don't pad the
/// response with zero tallies and null averages.
#[derive(Default)]
struct AgentDashboardMetrics {
    dispatch_count: u64,
    success_count: u64,
    failure_count: u64,
    elapsed_ms_total: u64,
    elapsed_count: u64,
    cost_usd_total: f64,
}

impl AgentDashboardMetrics {
    /// Project to the dashboard wire object, omitting fields that carry no
    /// signal: zero success/failure tallies, a null average (no terminal task
    /// yet), and zero attributed cost are all dropped rather than serialized.
    fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("dispatch_count".into(), json!(self.dispatch_count));
        if self.success_count > 0 {
            obj.insert("success_count".into(), json!(self.success_count));
        }
        if self.failure_count > 0 {
            obj.insert("failure_count".into(), json!(self.failure_count));
        }
        if self.elapsed_count > 0 {
            let avg = (self.elapsed_ms_total as f64) / (self.elapsed_count as f64);
            obj.insert("avg_elapsed_ms".into(), json!(avg));
        }
        let cost = (self.cost_usd_total * 10000.0).round() / 10000.0;
        if cost > 0.0 {
            obj.insert("cost_usd_total".into(), json!(cost));
        }
        Value::Object(obj)
    }
}

#[tool_router(router = roster_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_dashboard",
        description = "Page recent task summaries for lookup; do not take over another operator's task. Reports expand through bro_status. Context occupancy is not remaining work capacity."
    )]
    pub(crate) fn bro_dashboard(
        &self,
        Parameters(p): Parameters<DashboardParams>,
    ) -> CallToolResult {
        // Dashboard row cap — tasks are sorted by `started_at` descending and
        // truncated to `limit` rows (default 20). A completed task that falls
        // below this horizon is NOT reaped — it remains in the store and the
        // roster view. Raise the limit or filter by status/provider to see it.
        // If `bro_prune(task_ids=[...])` also cannot find the task, the daemon
        // has restarted and `TaskStore::load` dropped it via the task_ttl_ms
        // retention cutoff (default 24 h from `started_at`).
        let limit = p.limit.unwrap_or(20).clamp(1, 100);
        let offset = p.offset.unwrap_or(0);

        let filter_provider = match p
            .provider
            .as_deref()
            .map(str::parse::<Provider>)
            .transpose()
        {
            Ok(provider) => provider,
            Err(_) => {
                return Self::err_text(
                    "Invalid provider filter; use bro_providers to list provider names",
                );
            }
        };
        let filter_status = match p.status.as_deref() {
            None => None,
            Some("pending") => Some(bro_protocol::TaskStatus::Pending),
            Some("running") => Some(bro_protocol::TaskStatus::Running),
            Some("completed") => Some(bro_protocol::TaskStatus::Completed),
            Some("failed") => Some(bro_protocol::TaskStatus::Failed),
            Some("cancelled") => Some(bro_protocol::TaskStatus::Cancelled),
            Some(_) => {
                return Self::err_text(
                    "Invalid status filter; use pending, running, completed, failed, or cancelled",
                );
            }
        };

        let team_task_ids: Option<std::collections::HashSet<String>> = match p.team.as_ref() {
            None => None,
            Some(name) => match orchestration::team::load_team(name, &self.state.store_dir) {
                Some(team) => Some(
                    team.members
                        .iter()
                        .flat_map(|member| member.task_history.clone())
                        .collect(),
                ),
                None => {
                    return Self::err_text(
                        "Team filter could not be loaded; use bro_team(action=list) to select an existing team",
                    );
                }
            },
        };

        // Wave 7c: read the materialized RosterView snapshot instead
        // of iterating `task_store` and locking every per-task inner
        // mutex. `RosterEventSink::emit_*` keeps the view fresh at
        // the same call sites that touch `TaskInner`, so a snapshot
        // serves the same fields the legacy projection read under
        // the lock — without contending with event ingest on busy
        // tasks (invariant I6 of design/daemon-runtime/concurrency-
        // model.md). The team lookup is a per-call filesystem scan
        // (does not take a per-task inner mutex).
        let snapshot = self.state.roster_view.snapshot();
        let store_dir = self.state.store_dir.clone();

        let mut selected: Vec<_> = snapshot
            .into_iter()
            .filter(|s| {
                if let Some(fp) = filter_provider {
                    if s.provider != fp {
                        return false;
                    }
                }
                if let Some(fs) = filter_status {
                    if s.status != fs {
                        return false;
                    }
                }
                if let Some(ref ids) = team_task_ids {
                    if !ids.contains(s.task_id.as_str()) {
                        return false;
                    }
                }
                true
            })
            .collect();
        let total = selected.len();
        selected.sort_by(|a, b| {
            b.started_at
                .or(b.last_event_at)
                .unwrap_or(0)
                .cmp(&a.started_at.or(a.last_event_at).unwrap_or(0))
                .then_with(|| a.task_id.as_str().cmp(b.task_id.as_str()))
        });
        let mut agent_metrics: BTreeMap<String, AgentDashboardMetrics> = BTreeMap::new();
        let mut context_hint = None;
        let entries: Vec<Value> = selected
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|s| {
                let task_id_str = s.task_id.as_str().to_string();
                let bro_name =
                    orchestration::team::find_bro_name_for_task(&task_id_str, &store_dir);

                // Agent attribution rollup. The summary carries
                // `agent_label` directly (wave 7c DTO extension);
                // a missing label means "no agent attribution",
                // matching the legacy semantics where only
                // `inner.agent_label.is_some()` rolled into the
                // agents map.
                if let Some(label) = s.agent_label.as_ref() {
                    let metrics = agent_metrics.entry(label.clone()).or_default();
                    metrics.dispatch_count += 1;
                    match s.status {
                        bro_protocol::TaskStatus::Completed => metrics.success_count += 1,
                        bro_protocol::TaskStatus::Failed | bro_protocol::TaskStatus::Cancelled => {
                            metrics.failure_count += 1;
                        }
                        bro_protocol::TaskStatus::Running | bro_protocol::TaskStatus::Pending => {}
                    }
                    if s.status.is_terminal() {
                        if let (Some(start), Some(end)) = (s.started_at, s.last_event_at) {
                            metrics.elapsed_ms_total += end.saturating_sub(start);
                            metrics.elapsed_count += 1;
                        }
                    }
                    if let Some(cost) = s.cost {
                        metrics.cost_usd_total += cost;
                    }
                }

                // Recompute `elapsed` from summary timestamps so the
                // dashboard row matches the legacy projection
                // (terminal: `last_event_at - started_at`; live:
                // `now - started_at`).
                let elapsed = match (s.started_at, s.last_event_at) {
                    (Some(start), Some(end)) if s.status.is_terminal() => {
                        orch::format_elapsed(start, Some(end))
                    }
                    (Some(start), _) => orch::format_elapsed(start, None),
                    _ => "0s".to_string(),
                };

                let session_id_str = s.session_id.as_ref().map(|s| s.as_str().to_string());
                let mut entry = json!({
                    "taskId": task_id_str,
                    "provider": s.provider,
                    "sessionId": session_id_str,
                    "status": s.status,
                    "elapsed": elapsed,
                    "hasResult": s.status.is_terminal() && s.last_message_snippet.is_some(),
                    "hasLastMessage": s.last_message_snippet.is_some(),
                });
                if let Some(name) = bro_name {
                    entry["bro"] = Value::String(name);
                }
                if let Some(ref label) = label_from_summary(&s) {
                    entry["broLabel"] = Value::String(label.clone());
                }
                if let Some(ref label) = s.agent_label {
                    entry["agentLabel"] = Value::String(label.clone());
                }
                if let Some(ref report) = s.report_full {
                    entry["report"] = bro_report_v1_to_dashboard_json(report);
                }
                if s.interrupted {
                    entry["interrupted"] = Value::Bool(true);
                }
                // Share the status observation: context occupancy does not
                // determine whether a session can accept more work.
                if let Some(pressure) = s.context {
                    let mut observation = pressure.observation_json();
                    context_hint = observation
                        .as_object_mut()
                        .and_then(|value| value.remove("guidance"));
                    entry["context"] = observation;
                }
                entry
            })
            .collect();
        let agents: BTreeMap<String, Value> = agent_metrics
            .into_iter()
            .map(|(label, metrics)| (label, metrics.to_json()))
            .collect();

        let mut response = json!({"count": entries.len(), "total": total, "offset": offset, "tasks": entries, "agents": agents});
        let next_offset = offset.saturating_add(entries.len());
        if next_offset < total {
            response["next_offset"] = json!(next_offset);
        }
        if let Some(hint) = context_hint {
            response["context_hint"] = hint;
        }
        Self::ok_json(&response)
    }

    #[tool(
        name = "bro_report",
        description = "Attach the latest progress report to a task."
    )]
    pub(crate) fn bro_report(&self, Parameters(p): Parameters<ReportParams>) -> CallToolResult {
        let message = p.message.trim();
        if message.is_empty() {
            return Self::err_text("message is required");
        }

        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(task) => task,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };

        let report = orch::BroReport {
            message: message.to_string(),
            needs: p.needs.and_then(|needs| {
                let trimmed = needs.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }),
            data: p.data,
            reported_at: orch::now_ms(),
        };

        {
            let mut inner = task.inner.lock();
            inner.report = Some(report.clone());
        }
        crate::orchestration::request_persist(&self.state.task_store, &self.state.store_dir);

        Self::ok_json(&json!({
            "taskId": p.task_id,
            "report": report.to_json(),
        }))
    }

    #[tool(
        name = "bro_providers",
        description = "List provider summaries; pass provider to list its model slugs and reasoning efforts."
    )]
    pub(crate) fn bro_providers(
        &self,
        Parameters(params): Parameters<ProvidersParams>,
    ) -> CallToolResult {
        let selected = match params
            .provider
            .as_deref()
            .map(str::parse::<Provider>)
            .transpose()
        {
            Ok(Some(provider)) if !Provider::ALL.contains(&provider) => {
                return Self::err_text(
                    "Unknown dispatch provider; omit provider to list valid providers",
                );
            }
            Ok(provider) => provider,
            Err(_) => {
                return Self::err_text("Unknown provider; omit provider to list valid providers");
            }
        };
        let mut info = serde_json::Map::new();
        for p in Provider::ALL {
            if selected.is_some_and(|selected| selected != *p) {
                continue;
            }
            let mut entry = json!({
                "promptCache": p.prompt_cache(),
                "supportsResume": p.supports_resume(),
            });
            entry["modelCount"] = json!(p.models().len());
            entry["defaultModel"] = json!(p.models().iter().find(|m| m.default).map(|m| m.id));
            if selected.is_some() && !p.models().is_empty() {
                entry["models"] = serde_json::to_value(p.models()).unwrap_or_default();
            }
            if selected.is_some() && !p.efforts().is_empty() {
                entry["efforts"] = serde_json::to_value(p.efforts()).unwrap_or_default();
            }
            info.insert(p.as_str().to_string(), entry);
        }
        Self::ok_json(&Value::Object(info))
    }

    #[tool(
        name = "bro_brofile",
        description = "Manage brofiles and accounts. list returns paginated summaries; get by name returns the full lens and configuration."
    )]
    pub(crate) fn bro_brofile(&self, Parameters(p): Parameters<BrofileParams>) -> CallToolResult {
        use orchestration::brofile;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "create" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let filters = extra_filters_from_params(
                    p.allow_tools.as_deref(),
                    p.disallow_tools.as_deref(),
                );
                let bf = brofile::Brofile {
                    name: name.clone(),
                    provider,
                    account: p.account.clone(),
                    lens: p.lens.clone(),
                    model: p.model.clone(),
                    effort: p.effort.clone(),
                    tool_defaults: p.tool_defaults.clone(),
                    filters,
                    surface: p.surface.clone(),
                    coerce_workspace: p.coerce_workspace,
                    runtime: None,
                    context: p.context.clone(),
                    code_mode: p.code_mode,
                    service_tier: p.service_tier.clone(),
                };
                if let Err(e) =
                    brofile::save_brofile(&bf, scope, store_dir, p.project_dir.as_deref())
                {
                    return Self::err_text(&format!("brofile save failed: {e}"));
                }
                Self::ok_json(&json!({"created": name, "scope": scope, "brofile": bf}))
            }
            "list" => {
                let list = brofile::list_brofiles(scope, store_dir, p.project_dir.as_deref());
                let provider = match p
                    .provider
                    .as_deref()
                    .map(str::parse::<Provider>)
                    .transpose()
                {
                    Ok(provider) => provider,
                    Err(_) => return Self::err_text("Unknown provider"),
                };
                Self::ok_json(&brofile::list_summary_page(
                    list,
                    provider,
                    p.name.as_deref(),
                    p.offset.unwrap_or(0),
                    p.limit.unwrap_or(20),
                ))
            }
            "get" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match brofile::resolve_brofile(name, store_dir, p.project_dir.as_deref()) {
                    Some(bf) => Self::ok_json(&serde_json::to_value(&bf).unwrap_or_default()),
                    None => Self::err_text(&format!("Brofile not found: {name}")),
                }
            }
            "delete" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if brofile::delete_brofile(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Brofile not found: {name}"))
                }
            }
            "set_account" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match brofile::update_config(store_dir, |config| {
                    let account = config.accounts.entry(name.clone()).or_default();
                    account.env = p.env.clone();
                    account.response_view()
                }) {
                    Ok(configuration) => Self::ok_json(&json!({
                        "account": name, "updated": true, "configuration": configuration,
                    })),
                    Err(error) => Self::err_text(&format!("Configuration was not saved: {error}")),
                }
            }

            "list_accounts" => {
                let config = brofile::load_config(store_dir);
                Self::ok_json(&Value::Object(
                    config
                        .accounts
                        .iter()
                        .map(|(name, account)| (name.clone(), account.response_view()))
                        .collect(),
                ))
            }
            "set_provider_default" => {
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let account = match &p.account {
                    Some(a) if !a.trim().is_empty() => a.trim().to_string(),
                    _ => return Self::err_text("account is required"),
                };
                if let Err(error) = brofile::update_config(store_dir, |config| {
                    config.provider_defaults.insert(
                        provider,
                        brofile::ProviderDefault {
                            account: account.clone(),
                        },
                    );
                }) {
                    return Self::err_text(&format!("Configuration was not saved: {error}"));
                }
                Self::ok_json(
                    &json!({"provider": provider.as_str(), "account": account, "updated": true}),
                )
            }
            "get_provider_default" => {
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let account = brofile::provider_default_account(provider, store_dir);
                Self::ok_json(&json!({"provider": provider.as_str(), "account": account}))
            }
            "list_provider_defaults" => {
                let config = brofile::load_config(store_dir);
                let defaults: std::collections::HashMap<String, String> = config
                    .provider_defaults
                    .into_iter()
                    .map(|(provider, entry)| (provider.to_string(), entry.account))
                    .collect();
                Self::ok_json(&serde_json::to_value(defaults).unwrap_or_default())
            }
            "clear_provider_default" => {
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                match brofile::update_config(store_dir, |config| {
                    config.provider_defaults.remove(&provider).is_some()
                }) {
                    Ok(removed) => {
                        Self::ok_json(&json!({"provider": provider.as_str(), "removed": removed}))
                    }
                    Err(error) => Self::err_text(&format!("Configuration was not saved: {error}")),
                }
            }

            _ => Self::err_text(&format!("Unknown brofile action: {}", p.action)),
        }
    }

    #[tool(
        name = "bro_team",
        description = "Manage teamplates and teams. list/list_templates/roster return bounded summaries; get/get_template return exact JSON body pages."
    )]
    pub(crate) async fn bro_team(&self, Parameters(p): Parameters<TeamParams>) -> CallToolResult {
        if let Err(error) =
            validate_team_params(&p).and_then(|()| require_team_template_locality(self, &p))
        {
            return Self::err_text(&error.to_string());
        }
        if matches!(
            p.action.as_str(),
            "list" | "list_templates" | "roster" | "get" | "get_template"
        ) {
            let server = self.clone();
            return Self::run_blocking_with_structured("bro_team", move || {
                let value = team_discovery(&server, &p)?;
                Ok((serde_json::to_string(&value)?, value))
            })
            .await;
        }
        use orchestration::team;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");
        let source_project_dir = team_source_project_dir(self, p.project_dir.as_deref());

        match p.action.as_str() {
            "save_template" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let members = match &p.members {
                    Some(m) if !m.is_empty() => m,
                    _ => return Self::err_text("members is required"),
                };
                // Validate brofile names
                for m in members {
                    if orchestration::brofile::resolve_brofile(
                        &m.brofile,
                        store_dir,
                        source_project_dir,
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let advisor = match self.resolve_team_advisor_config(
                    p.advisor.as_ref(),
                    store_dir,
                    source_project_dir,
                ) {
                    Ok(cfg) => cfg,
                    Err(e) => return Self::err_text(&e),
                };
                let tp = team::Teamplate {
                    name: name.clone(),
                    members: members
                        .iter()
                        .map(|m| team::TeamplateMember {
                            brofile: m.brofile.clone(),
                            alias: m.alias.clone(),
                            count: m.count.unwrap_or(1),
                        })
                        .collect(),
                    advisor,
                    diversity_floor: None,
                };
                if let Err(error) =
                    team::save_teamplate(&tp, scope, store_dir, p.project_dir.as_deref())
                {
                    return Self::err_text(&format!("Teamplate was not saved: {error}"));
                }
                Self::ok_json(&json!({"saved": name, "scope": scope}))
            }
            "delete_template" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if team::delete_teamplate(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Teamplate not found: {name}"))
                }
            }
            "create" => {
                let template = match &p.template {
                    Some(t) => t,
                    None => return Self::err_text("template is required"),
                };
                let tp = match team::resolve_teamplate(template, store_dir, source_project_dir) {
                    Some(tp) => tp,
                    None => {
                        return Self::err_text(&format!(
                            "Teamplate not found in the available source: {template}. In catalog mode create uses daemon-owned global templates; use bro_team(action=list_templates, scope=global), or inspect a project template through the checkout owner's file tools and save an approved global template. project_dir supplies worker context, not template read authority"
                        ));
                    }
                };
                // Catalog-mode creation uses only global configuration. The
                // project association is still retained for worker dispatch.
                if let Err(error) = validate_team_object_name(&tp.name) {
                    return Self::err_text(&error.to_string());
                }
                for m in &tp.members {
                    if let Err(error) = validate_team_object_name(&m.brofile) {
                        return Self::err_text(&error.to_string());
                    }
                    if orchestration::brofile::resolve_brofile(
                        &m.brofile,
                        store_dir,
                        source_project_dir,
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let advisor_override = match self.resolve_team_advisor_config(
                    p.advisor.as_ref(),
                    store_dir,
                    source_project_dir,
                ) {
                    Ok(cfg) => cfg,
                    Err(e) => return Self::err_text(&e),
                };
                if let Some(advisor) = advisor_override.as_ref().or(tp.advisor.as_ref()) {
                    if let Err(error) = validate_team_object_name(&advisor.brofile) {
                        return Self::err_text(&error.to_string());
                    }
                    if orchestration::brofile::resolve_brofile(
                        &advisor.brofile,
                        store_dir,
                        source_project_dir,
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!(
                            "Advisor brofile not found in the available source: {}; catalog teams require a daemon-owned global brofile",
                            advisor.brofile
                        ));
                    }
                }
                let team_name = p
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{template}-{}", orch::now_ms()));
                let mut tp = tp;
                if advisor_override.is_some() {
                    tp.advisor = advisor_override;
                }
                let mut t = match team::instantiate_team(
                    &tp,
                    &team_name,
                    p.project_dir.as_deref(),
                    store_dir,
                ) {
                    Ok(team) => team,
                    Err(error) => return Self::err_text(&format!("Team was not saved: {error}")),
                };
                if let Err(e) = self.initialize_team_advisor(&mut t).await {
                    return Self::err_text(&e);
                }
                Self::ok_json(&team_create_receipt(
                    &t,
                    !self.state.project_authority.is_bridge(),
                ))
            }
            "dissolve" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                if p.cancel_running.unwrap_or(false) {
                    let task_store = self.state.task_store.read();
                    for member in &loaded_team.members {
                        for tid in &member.task_history {
                            if let Some(task) = task_store.get(tid) {
                                let _ = orch::cancel_task(
                                    &task,
                                    &self.state.task_store,
                                    &self.state.store_dir,
                                );
                            }
                        }
                    }
                }
                team::remove_team(name, store_dir);
                Self::ok_json(&json!({"dissolved": name}))
            }
            _ => Self::err_text(&format!("Unknown team action: {}", p.action)),
        }
    }

    pub(crate) fn resolve_team_advisor_config(
        &self,
        advisor: Option<&AdvisorSpecParams>,
        store_dir: &Path,
        project_dir: Option<&str>,
    ) -> Result<Option<orchestration::team::TeamAdvisorConfig>, String> {
        let Some(advisor) = advisor else {
            return Ok(None);
        };
        if advisor.charter.trim().is_empty() {
            return Err("advisor.charter is required and cannot be empty".into());
        }
        let brofile =
            orchestration::brofile::resolve_brofile(&advisor.brofile, store_dir, project_dir)
                .ok_or_else(|| format!("Brofile not found: {}", advisor.brofile))?;
        if !brofile.provider.supports_resume() {
            return Err(format!(
                "Advisor brofile {} uses provider {} which does not support resume",
                advisor.brofile, brofile.provider
            ));
        }
        Ok(Some(orchestration::team::TeamAdvisorConfig {
            brofile: advisor.brofile.clone(),
            alias: advisor.alias.clone(),
            charter: advisor.charter.clone(),
            context: advisor.context.clone(),
            halt_conditions: advisor.halt_conditions.clone().unwrap_or_default(),
            exit_conditions: advisor.exit_conditions.clone().unwrap_or_default(),
            packet_id: advisor.packet_id.clone(),
            timeout_seconds: advisor.timeout_seconds,
            mode: advisor.mode.unwrap_or_default(),
        }))
    }

    pub(crate) fn build_team_advisor_init_prompt(
        &self,
        team: &orchestration::team::Team,
        advisor: &orchestration::team::TeamAdvisor,
    ) -> String {
        let member_list = team
            .members
            .iter()
            .map(|m| format!("- {} ({})", m.name, m.brofile))
            .collect::<Vec<_>>()
            .join("\n");
        let halt_list = if advisor.config.halt_conditions.is_empty() {
            "- none declared".to_string()
        } else {
            advisor
                .config
                .halt_conditions
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let exit_list = if advisor.config.exit_conditions.is_empty() {
            "- none declared".to_string()
        } else {
            advisor
                .config
                .exit_conditions
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let context = advisor.config.context.as_deref().unwrap_or("(none)");
        let packet_id = advisor.config.packet_id.as_deref().unwrap_or("(none)");
        format!(
            "You are the advisor for team \"{team_name}\".\n\n\
Role:\n\
- monitor big-picture progression only\n\
- stay out of code-level execution unless explicitly asked\n\
- use the charter, halt conditions, exit conditions, and packet result to steer\n\
- when the checkpoint indicates drift/blockage/exit, say so plainly\n\n\
Team members:\n{member_list}\n\n\
Charter:\n{charter}\n\n\
Context:\n{context}\n\n\
Halt conditions:\n{halt_list}\n\n\
Exit conditions:\n{exit_list}\n\n\
Compiled packet for mechanical evaluation:\n- {packet_id}\n\n\
From now on, you will receive structured checkpoint updates after wait boundaries.\n\
Respond tersely with:\n\
Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO\n\
Rationale: <1-3 sentences>\n\
Next step: <one concrete steering suggestion>\n",
            team_name = team.name,
            member_list = member_list,
            charter = advisor.config.charter,
            context = context,
            halt_list = halt_list,
            exit_list = exit_list,
            packet_id = packet_id,
        )
    }

    pub(crate) async fn dispatch_team_advisor_prompt(
        &self,
        team: &mut orchestration::team::Team,
        prompt: String,
    ) -> Result<(Arc<orch::Task>, Option<f64>), String> {
        let advisor = match team.advisor.as_mut() {
            Some(a) => a,
            None => return Err("team has no advisor configured".into()),
        };
        validate_team_object_name(&advisor.config.brofile).map_err(|error| error.to_string())?;
        let store_dir = self.state.store_dir.clone();
        let brofile = orchestration::brofile::resolve_brofile(
            &advisor.config.brofile,
            &store_dir,
            team_source_project_dir(self, team.project_dir.as_deref()),
        )
        .ok_or_else(|| format!("Brofile not found: {}", advisor.config.brofile))?;
        let provider = brofile.provider;
        // For an advisor resume, prefer the lease-captured policy from
        // the original dispatch over the current brofile — resume must
        // honor dispatch-time suppression intent.
        let advisor_lease = advisor
            .session_id
            .as_deref()
            .filter(|s| !s.is_empty() && *s != "pending")
            .and_then(|sid| {
                orchestration::allocator::lookup_lease_for_session_any_provider(
                    &store_dir,
                    &self.state.task_store.read(),
                    sid,
                )
            });
        let effective_provider = advisor_lease
            .as_ref()
            .map(|lease| lease.provider)
            .unwrap_or(provider);
        let effective_context = advisor_lease
            .as_ref()
            .and_then(|l| l.brofile_context.as_ref())
            .or(brofile.context.as_ref());
        orchestration::brofile::enforce_provider_defaults(effective_provider, effective_context)?;
        let env_overrides = orchestration::brofile::resolve_provider_env(
            effective_provider,
            advisor_lease
                .as_ref()
                .and_then(|l| l.account.as_deref())
                .or(brofile.account.as_deref()),
            advisor_lease
                .as_ref()
                .and_then(|l| l.model.as_deref())
                .or(brofile.model.as_deref()),
            &store_dir,
            effective_context,
        );
        let exec_opts = if let Some(lease) = advisor_lease.as_ref() {
            orchestration::allocator::exec_opts_for_lane(&orchestration::allocator::RuntimeLane {
                provider: lease.provider,
                account: lease.account.clone(),
                tier: lease.tier.clone(),
                model: lease.model.clone(),
                effort: lease.effort.clone(),
                capabilities: lease.capabilities.clone(),
            })
        } else if brofile.model.is_some()
            || brofile.effort.is_some()
            || brofile.code_mode.is_some()
            || brofile.service_tier.is_some()
        {
            Some(ExecOpts {
                model: brofile.model.clone(),
                effort: brofile.effort.clone(),
                provider_defaults: None,
                code_mode: brofile.code_mode,
                service_tier: brofile.service_tier.clone(),
                output_schema: None,
            })
        } else {
            None
        };
        let exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            effective_context,
        );
        let task_id = uuid::Uuid::new_v4().to_string();
        let timeout = advisor.config.timeout_seconds;
        let cwd = team.project_dir.clone();
        let task = match advisor.session_id.as_deref() {
            Some("pending") => {
                return Err(format!(
                    "Advisor {} is still waiting for session discovery; refusing to launch a second session",
                    advisor.name
                ));
            }
            Some(session_id) => {
                let resume_lease = try_acquire_resume_lease(
                    &self.state.task_store,
                    self.state.resume_leases.as_ref(),
                    effective_provider,
                    session_id,
                )?;
                let ambient_ctx = orch::AmbientContext {
                    task_id: Some(task_id.clone()),
                    session_id: Some(session_id.to_string()),
                    project_dir: cwd.clone(),
                    bro_name: Some(advisor.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    pin_block: self.ambient_pin_block(
                        cwd.as_deref(),
                        Some(advisor.name.as_str()),
                        Some(session_id),
                        None,
                        None,
                    ),
                    completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
                    allow_recursion: false,
                    provider: Some(effective_provider),
                    coerce_workspace: brofile.coerce_workspace.unwrap_or(false),
                };
                // Resume passes the full dispatch context, persona included
                // (dispatch-prompt-slots.md §6).
                let dispatch_context = ambient_ctx.dispatch_context(brofile.lens.as_deref());
                let mut args = effective_provider.build_resume_args(
                    session_id,
                    &prompt,
                    Some(&dispatch_context),
                    exec_opts.as_ref(),
                );
                let dispatch_filters = match resolve_dispatch_filters(
                    effective_provider,
                    cwd.as_deref(),
                    false,
                    &task_id,
                    brofile.filters.as_ref(),
                ) {
                    Ok(df) => df,
                    Err(e) => return Err(e),
                };
                args.extend(dispatch_filters.args);
                let task = orch::spawn_task(
                    task_id.clone(),
                    effective_provider,
                    args,
                    session_id.to_string(),
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                    Some(self.state.roster_events()),
                    None,
                    None,
                    Some(self.state.system_events.clone()),
                    // dispatch_team_advisor_prompt — team advisor
                    // resume branch, workflow origin (advisor
                    // runtime is team-orchestration traffic).
                    bro_core::Origin::Workflow,
                )
                .await;
                cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
                release_resume_lease_when_done(task.clone(), resume_lease);
                task
            }
            None => {
                let session_id = "pending".to_string();
                let ambient_ctx = orch::AmbientContext {
                    task_id: Some(task_id.clone()),
                    session_id: Some(session_id.clone()),
                    project_dir: cwd.clone(),
                    bro_name: Some(advisor.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    pin_block: self.ambient_pin_block(
                        cwd.as_deref(),
                        Some(advisor.name.as_str()),
                        Some(session_id.as_str()),
                        None,
                        None,
                    ),
                    completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
                    allow_recursion: false,
                    provider: Some(provider),
                    coerce_workspace: brofile.coerce_workspace.unwrap_or(false),
                };
                let dispatch_context = ambient_ctx.dispatch_context(brofile.lens.as_deref());
                let mut args = provider.build_exec_args(
                    &prompt,
                    Some(&dispatch_context),
                    &session_id,
                    cwd.as_deref(),
                    exec_opts.as_ref(),
                );
                let dispatch_filters = match resolve_dispatch_filters(
                    provider,
                    cwd.as_deref(),
                    false,
                    &task_id,
                    brofile.filters.as_ref(),
                ) {
                    Ok(df) => df,
                    Err(e) => return Err(e),
                };
                args.extend(dispatch_filters.args);
                let task = orch::spawn_task(
                    task_id.clone(),
                    provider,
                    args,
                    session_id,
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                    Some(self.state.roster_events()),
                    None,
                    None,
                    Some(self.state.system_events.clone()),
                    // dispatch_team_advisor_prompt — team advisor
                    // fresh branch, workflow origin.
                    bro_core::Origin::Workflow,
                )
                .await;
                cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
                task
            }
        };

        advisor.task_history.push(task_id);
        advisor.session_id = Some(task.inner.lock().session_id.clone());
        orchestration::team::save_team(team, &self.state.store_dir);
        Ok((task, timeout))
    }

    pub(crate) fn persist_advisor_session_to_team(&self, team_name: &str, task: &Arc<orch::Task>) {
        let Some(mut team) = orchestration::team::load_team(team_name, &self.state.store_dir)
        else {
            return;
        };
        let Some(advisor) = team.advisor.as_mut() else {
            return;
        };
        let session_id = task.inner.lock().session_id.clone();
        if session_id != "pending" {
            advisor.session_id = Some(session_id);
            orchestration::team::save_team(&team, &self.state.store_dir);
        }
    }

    pub(crate) async fn await_team_advisor_task(
        &self,
        team_name: &str,
        task: Arc<orch::Task>,
        timeout: Option<f64>,
    ) -> Result<Value, String> {
        let completed = orch::wait_for_task_with_timeout(&task, timeout).await;
        self.persist_advisor_session_to_team(team_name, &task);
        Ok(if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        })
    }

    pub(crate) async fn initialize_team_advisor(
        &self,
        team: &mut orchestration::team::Team,
    ) -> Result<(), String> {
        let Some(advisor) = team.advisor.as_ref() else {
            return Ok(());
        };
        if advisor
            .session_id
            .as_deref()
            .filter(|s| *s != "pending")
            .is_some()
        {
            return Ok(());
        }
        let prompt = self.build_team_advisor_init_prompt(team, advisor);
        let team_name = team.name.clone();
        let (task, timeout) = self.dispatch_team_advisor_prompt(team, prompt).await?;
        let _ = self
            .await_team_advisor_task(&team_name, task, timeout)
            .await?;
        Ok(())
    }

    pub(crate) fn summarize_notes_for_tasks(&self, task_ids: &[String]) -> AdvisorNoteSummary {
        use notes::{NoteKind, NoteResolution};

        let mut summary = AdvisorNoteSummary::default();
        let task_set: std::collections::HashSet<&str> =
            task_ids.iter().map(String::as_str).collect();
        let mut recent_unresolved = Vec::new();

        for note in self.state.notes.read().all().iter().rev() {
            let Some(task_id) = note.task_id.as_deref() else {
                continue;
            };
            if !task_set.contains(task_id) {
                continue;
            }
            match note.kind {
                NoteKind::Dispute => summary.dispute_count += 1,
                NoteKind::Assumption => summary.assumption_count += 1,
                NoteKind::Surprise => summary.surprise_count += 1,
                NoteKind::Followup => summary.followup_count += 1,
                NoteKind::Blocked => summary.blocked_count += 1,
                NoteKind::Learned => summary.learned_count += 1,
                NoteKind::Done => summary.done_count += 1,
            }
            if note.resolution == NoteResolution::Unresolved && recent_unresolved.len() < 5 {
                recent_unresolved.push(format!("{}: {}", note.kind.as_ref(), note.body));
            }
        }
        summary.recent_unresolved = recent_unresolved;
        summary
    }

    pub(crate) fn build_advisor_checkpoint(
        &self,
        team: &orchestration::team::Team,
        wait_kind: &str,
        results: &[Value],
    ) -> AdvisorCheckpoint {
        let monitored_task_ids: Vec<String> = results
            .iter()
            .filter_map(|r| {
                r.get("taskId")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .collect();
        let notes = self.summarize_notes_for_tasks(&monitored_task_ids);
        let mut members = Vec::new();
        let mut completed_count = 0usize;
        let mut failed_count = 0usize;
        let mut cancelled_count = 0usize;
        let mut timed_out_count = 0usize;
        let mut running_count = 0usize;

        for result in results {
            let status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let timed_out = result.get("timed_out").is_some();
            if timed_out {
                timed_out_count += 1;
                running_count += 1;
            } else {
                match status.as_str() {
                    "completed" | "Completed" => completed_count += 1,
                    "failed" | "Failed" => failed_count += 1,
                    "cancelled" | "Cancelled" => cancelled_count += 1,
                    _ => running_count += 1,
                }
            }
            let result_snippet = result
                .get("result")
                .and_then(Value::as_str)
                .map(|s| s.trim().replace('\n', " "))
                .filter(|s| !s.is_empty())
                .map(|s| {
                    if s.len() > 160 {
                        format!("{}…", &s[..160])
                    } else {
                        s
                    }
                })
                .or_else(|| {
                    result
                        .get("lastAssistantSnippet")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                });
            members.push(AdvisorMemberCheckpoint {
                bro: result
                    .get("bro")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                task_id: result
                    .get("taskId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status,
                timed_out,
                keep_going: result
                    .get("keep_going")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                result_snippet,
            });
        }

        AdvisorCheckpoint {
            wait_kind: wait_kind.to_string(),
            team_name: team.name.clone(),
            teamplate: team.teamplate.clone(),
            packet_id: team
                .advisor
                .as_ref()
                .and_then(|a| a.config.packet_id.clone()),
            monitored_task_ids,
            total_count: results.len(),
            completed_count,
            failed_count,
            cancelled_count,
            timed_out_count,
            running_count,
            dispute_count: notes.dispute_count,
            assumption_count: notes.assumption_count,
            surprise_count: notes.surprise_count,
            followup_count: notes.followup_count,
            blocked_count: notes.blocked_count,
            learned_count: notes.learned_count,
            done_count: notes.done_count,
            members,
            notes,
        }
    }

    pub(crate) fn apply_advisor_packet(
        &self,
        packet_id: &str,
        checkpoint: &AdvisorCheckpoint,
    ) -> Result<Value, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store.load(packet_id).map_err(|e| format!("{e:#}"))?;
        let entity = serde_json::to_value(checkpoint).map_err(|e| e.to_string())?;
        let prediction = apply_packet_with(&packet, &entity, &*packet_store);
        Ok(match prediction {
            Some(prediction) => json!({
                "packetId": packet.id,
                "match": true,
                "ruleId": prediction.rule_id,
                "classification": prediction.classification,
                "consequent": prediction.consequent,
                "confidence": prediction.confidence,
            }),
            None => json!({
                "packetId": packet.id,
                "match": false,
            }),
        })
    }

    pub(crate) async fn maybe_resume_team_advisor(
        &self,
        team_name: &str,
        wait_kind: &str,
        results: &[Value],
    ) -> Result<Option<Value>, String> {
        let mut team = match orchestration::team::load_team(team_name, &self.state.store_dir) {
            Some(team) => team,
            None => return Ok(None),
        };
        let Some(advisor) = team.advisor.as_ref() else {
            return Ok(None);
        };
        let checkpoint = self.build_advisor_checkpoint(&team, wait_kind, results);
        let packet_eval = match advisor.config.packet_id.as_deref() {
            Some(packet_id) => Some(self.apply_advisor_packet(packet_id, &checkpoint)?),
            None => None,
        };
        let checkpoint_json =
            serde_json::to_string_pretty(&checkpoint).map_err(|e| e.to_string())?;
        let packet_section = packet_eval
            .as_ref()
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
            .unwrap_or_else(|| "{\n  \"configured\": false\n}".to_string());
        let prompt = format!(
            "Team wait checkpoint.\n\n\
Checkpoint entity:\n{checkpoint_json}\n\n\
Mechanical packet evaluation:\n{packet_section}\n\n\
Interpret the checkpoint against the charter and respond with:\n\
Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO\n\
Rationale: <1-3 sentences>\n\
Next step: <one concrete steering suggestion>\n"
        );
        let advisor_mode = advisor.config.mode;
        let team_name_owned = team.name.clone();
        let (task, timeout) = self.dispatch_team_advisor_prompt(&mut team, prompt).await?;
        let advisor_result = match advisor_mode {
            orchestration::team::AdvisorMode::Blocking => {
                let result = self
                    .await_team_advisor_task(&team_name_owned, task.clone(), timeout)
                    .await?;
                json!({
                    "mode": "blocking",
                    "taskId": task.id(),
                    "result": result,
                })
            }
            orchestration::team::AdvisorMode::Background => {
                let server = self.clone();
                let team_name = team_name_owned.clone();
                let task_clone = task.clone();
                tokio::spawn(async move {
                    let _ = server
                        .await_team_advisor_task(&team_name, task_clone, timeout)
                        .await;
                });
                let inner = task.inner.lock();
                json!({
                    "mode": "background",
                    "scheduled": true,
                    "taskId": inner.id,
                    "sessionId": inner.session_id,
                    "status": "running",
                })
            }
        };
        Ok(Some(json!({
            "checkpoint": checkpoint,
            "packet": packet_eval,
            "advisor": advisor_result,
        })))
    }

    pub(crate) fn record_task_to_bro(&self, bro_name: &str, task: &Arc<orch::Task>) {
        // Stamp the task with a default label up-front so brofile-only
        // dispatches (no team match) still surface in `bro tail` with a
        // name. Team-attributed dispatches will overwrite below with a
        // more precise `<team>::<member>` label.
        task.inner.lock().bro_label = Some(bro_name.to_string());

        let _lock = orchestration::team::lock_teams();
        let tid = task.id();
        let teams = orchestration::team::load_all_teams(&self.state.store_dir);
        let Ok(bro_match_opt) = orchestration::team::resolve_bro_selector(bro_name, &teams) else {
            return;
        };
        let Some(bro_match) = bro_match_opt else {
            return;
        };
        let target_team = bro_match.team.name.clone();
        let target_member_idx = bro_match.member_idx;
        let task_sid = task.inner.lock().session_id.clone();

        for mut team in teams {
            if team.name != target_team {
                continue;
            }
            let member = &mut team.members[target_member_idx];
            member.task_history.push(tid.clone());
            // Track the latest launch immediately. Fresh harness dispatches
            // should already carry a concrete pre-minted session id; legacy
            // pending values are still preserved so later team rounds fail
            // closed instead of forking a second session.
            member.session_id = Some(task_sid.clone());
            // Stamp a precise team::member label on the task so the
            // tail handler can attribute even when later resolution
            // (find_bro_ref_for_task) hits the duplicate-name
            // ambiguity case (two team members sharing a brofile).
            task.inner.lock().bro_label = Some(format!("{}::{}", team.name, member.name));
            orchestration::team::save_team(&team, &self.state.store_dir);
            break;
        }
    }
}

fn team_source_project_dir<'a>(
    server: &BlackboxServer,
    project: Option<&'a str>,
) -> Option<&'a str> {
    if server.state.project_authority.is_bridge() {
        project
    } else {
        None
    }
}

fn require_team_template_locality(server: &BlackboxServer, p: &TeamParams) -> anyhow::Result<()> {
    if matches!(
        p.action.as_str(),
        "save_template" | "delete_template" | "list_templates" | "get_template"
    ) && p.scope.as_deref() == Some("project")
        && !server.state.project_authority.is_bridge()
    {
        anyhow::bail!(
            "error.team_template_locality_required: project .bro/teamplates have no remote source lane; inspect or edit them with the checkout owner's file tools, or use daemon-owned templates with scope=global and no project_dir. No project template was read or changed; passing a caller path cannot grant daemon checkout access"
        );
    }
    Ok(())
}

fn validate_team_object_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0']),
        "name must be an exact stored team/template/brofile name, not a path"
    );
    Ok(())
}

fn team_create_receipt(team: &orchestration::team::Team, global_source: bool) -> Value {
    let mut receipt = json!({"created":team.name, "teamplate":team.teamplate, "memberCount":team.members.len(),
        "detail_hint":"bro_team(action=roster, name=<created>) for member pages; action=get for exact stored configuration and history"});
    if global_source {
        receipt["templateScope"] = json!("global");
    }
    if team.advisor.is_some() {
        receipt["hasAdvisor"] = json!(true);
    }
    receipt
}

/// Validate action-specific selectors before any store access. A missing
/// project selector must never become the daemon's current directory.
fn validate_team_params(p: &TeamParams) -> anyhow::Result<()> {
    let template_action = matches!(
        p.action.as_str(),
        "save_template" | "list_templates" | "get_template" | "delete_template"
    );
    let list = matches!(p.action.as_str(), "list" | "list_templates" | "roster");
    let exact = matches!(p.action.as_str(), "get" | "get_template");
    if !list && (p.limit.is_some() || p.offset.is_some()) {
        anyhow::bail!("limit and offset require list, list_templates, or roster");
    }
    if !exact && (p.cursor.is_some() || p.body_limit.is_some()) {
        anyhow::bail!("cursor and body_limit require get or get_template");
    }
    if template_action {
        match p.scope.as_deref().unwrap_or("global") {
            "global" if p.project_dir.is_some() => {
                anyhow::bail!("project_dir requires scope=project for template actions")
            }
            "global" => {}
            "project" => {
                let path = p.project_dir.as_deref().ok_or_else(|| anyhow::anyhow!("project_dir is required for scope=project; no daemon current-directory fallback"))?;
                anyhow::ensure!(
                    Path::new(path).is_absolute(),
                    "project template directory must be absolute"
                );
            }
            _ => anyhow::bail!("scope must be global or project"),
        }
    } else if p.scope.is_some() {
        anyhow::bail!(
            "scope applies only to template actions; use project_dir to filter live teams"
        );
    }
    if matches!(p.action.as_str(), "get" | "get_template" | "roster") && p.name.is_none() {
        anyhow::bail!("name is required");
    }
    if let Some(name) = p.name.as_deref() {
        validate_team_object_name(name)?;
    }
    if let Some(template) = p.template.as_deref() {
        validate_team_object_name(template)?;
    }
    if let Some(members) = &p.members {
        for member in members {
            validate_team_object_name(&member.brofile)?;
        }
    }
    if let Some(advisor) = &p.advisor {
        validate_team_object_name(&advisor.brofile)?;
    }
    if (list || exact)
        && (p.members.is_some()
            || p.template.is_some()
            || p.cancel_running.is_some()
            || p.advisor.is_some())
    {
        anyhow::bail!(
            "members, template, cancel_running, and advisor are mutation parameters, not discovery filters"
        );
    }
    Ok(())
}

fn team_advisor_summary(advisor: &orchestration::team::TeamAdvisor) -> Value {
    let mut row = json!({"name": advisor.name, "brofile": advisor.config.brofile, "mode": advisor.config.mode});
    if let Some(session) = &advisor.session_id {
        row["sessionId"] = json!(session);
    }
    if let Some(packet) = &advisor.config.packet_id {
        row["packetId"] = json!(packet);
    }
    if !advisor.task_history.is_empty() {
        row["taskCount"] = json!(advisor.task_history.len());
    }
    row
}

fn team_template_summary(template: &orchestration::team::Teamplate) -> Value {
    let brofiles = template
        .members
        .iter()
        .map(|member| member.brofile.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut row = json!({"name": template.name, "slotCount": template.members.len(),
        "memberCount": template.members.iter().map(|member| u64::from(member.count)).sum::<u64>(),
        "brofiles": brofiles.iter().take(3).collect::<Vec<_>>(),
    });
    if let Err(error) = orchestration::team::validate_teamplate_member_count(template) {
        row["admissionError"] = json!(error.to_string());
    }
    if brofiles.len() > 3 {
        row["omittedBrofileCount"] = json!(brofiles.len() - 3);
    }
    if let Some(floor) = template.diversity_floor {
        row["diversityFloor"] = json!(floor);
    }
    if let Some(advisor) = &template.advisor {
        row["advisor"] = json!({"name": advisor.display_name(), "brofile": advisor.brofile, "mode": advisor.mode});
    }
    row
}

fn team_summary_page(
    rows: Vec<Value>,
    field: &str,
    p: &TeamParams,
    mut metadata: Value,
) -> anyhow::Result<Value> {
    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    metadata["total"] = json!(rows.len());
    metadata["offset"] = json!(offset);
    metadata["limit"] = json!(limit);
    let selected = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    metadata["count"] = json!(selected.len());
    metadata[field] = json!(selected);
    bbox_corpus_core::response_page::bound_page(metadata, field)
}

fn team_discovery(server: &BlackboxServer, p: &TeamParams) -> anyhow::Result<Value> {
    use orchestration::team;
    let store = &server.state.store_dir;
    if matches!(p.action.as_str(), "list_templates" | "get_template") {
        let scope = p.scope.as_deref().unwrap_or("global");
        require_team_template_locality(server, p)?;
        if p.action == "get_template" {
            let name = p.name.as_deref().expect("validated template name");
            let template =
                team::get_teamplate_checked(name, scope, store, p.project_dir.as_deref())?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Teamplate not found; use list_templates in the same scope")
                    })?;
            let selection = json!(["teamplate", scope, p.project_dir, template.name]).to_string();
            return Ok(json!({"name":template.name, "scope":scope,
                "body":super::body_page::json_body_page(&selection, &serde_json::to_value(&template)?, p.cursor.as_deref(), p.body_limit)?}));
        }
        let mut templates = if let Some(name) = p.name.as_deref() {
            team::get_teamplate_checked(name, scope, store, p.project_dir.as_deref())?
                .into_iter()
                .collect()
        } else {
            team::list_teamplates_checked(scope, store, p.project_dir.as_deref())?
        };
        templates.sort_by(|a, b| a.name.cmp(&b.name));
        return team_summary_page(
            templates.iter().map(team_template_summary).collect(),
            "templates",
            p,
            json!({"scope":scope, "detail_hint":"bro_team(action=get_template, name=<name>, same scope/project_dir); follow body.next_cursor"}),
        );
    }
    if p.action == "list" {
        let mut teams = if let Some(name) = p.name.as_deref() {
            team::load_team_checked(name, store)?.into_iter().collect()
        } else {
            team::load_all_teams_checked(store)?
        };
        teams.retain(|row| {
            p.name.as_deref().is_none_or(|name| row.name == name)
                && p.project_dir
                    .as_deref()
                    .is_none_or(|project| row.project_dir.as_deref() == Some(project))
        });
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        let rows = teams.iter().map(|team| {
            let mut row = json!({"name":team.name, "teamplate":team.teamplate, "memberCount":team.members.len(), "createdAt":team.created_at});
            if let Some(project) = &team.project_dir { row["projectDir"] = json!(project); }
            if let Some(advisor) = &team.advisor { row["advisor"] = team_advisor_summary(advisor); }
            row
        }).collect();
        return team_summary_page(
            rows,
            "teams",
            p,
            json!({"detail_hint":"bro_team(action=roster, name=<name>) for members; action=get for exact JSON body pages. projectDir is a stored association, not a filesystem read handle."}),
        );
    }
    let name = p.name.as_deref().expect("validated exact team selector");
    let team = team::load_team_checked(name, store)?
        .ok_or_else(|| anyhow::anyhow!("Team not found; use bro_team(action=list)"))?;
    anyhow::ensure!(
        p.project_dir
            .as_deref()
            .is_none_or(|project| team.project_dir.as_deref() == Some(project)),
        "team does not match the exact project_dir filter"
    );
    if p.action == "get" {
        let selection = json!(["team", name, p.project_dir]).to_string();
        return Ok(
            json!({"name":name, "body":super::body_page::json_body_page(&selection, &serde_json::to_value(&team)?, p.cursor.as_deref(), p.body_limit)?}),
        );
    }
    let task_store = server.state.task_store.read();
    let mut members = team.members.iter().collect::<Vec<_>>();
    members.sort_by(|a, b| (&a.name, &a.brofile).cmp(&(&b.name, &b.brofile)));
    let rows = members.into_iter().map(|member| {
        let mut row = json!({"name":member.name, "brofile":member.brofile, "taskCount":member.task_history.len()});
        if let Some(session) = &member.session_id { row["sessionId"] = json!(session); }
        if let Some(id) = member.task_history.last() {
            if let Some(task) = task_store.get(id) {
                let inner = task.inner.lock();
                row["latestTask"] = json!({"taskId":inner.id, "status":inner.status, "provider":inner.provider,
                    "elapsed":orch::format_elapsed(inner.started_at, inner.completed_at)});
            } else {
                row["latestTask"] = json!({"taskId":id, "statusUnavailable":true});
            }
        }
        row
    }).collect();
    drop(task_store);
    let mut metadata = json!({"team":name, "teamplate":team.teamplate,
        "detail_hint":"bro_team(action=get, name=<team>) for stored advisor configuration and task history; follow body.next_cursor. Brofile configuration expands through bro_brofile(action=get, name=<brofile>)."});
    if let Some(advisor) = &team.advisor {
        metadata["advisor"] = team_advisor_summary(advisor);
    }
    team_summary_page(rows, "members", p, metadata)
}

/// Pick the `broLabel` value the legacy `bro_dashboard` row
/// surfaced. `RosterSummaryV1.label` collapses `bro_label` and
/// `agent_label` (one or the other) — but the dashboard
/// historically used `inner.bro_label` (the team-shaped identity)
/// when present. The summary's `name` field is the daemon display
/// name and can match, but `label` is the closest field-by-field
/// proxy. We read the same `label` slot the projection already
/// computed; the dashboard never relied on `agent_label` falling
/// into the `broLabel` row.
fn label_from_summary(s: &bro_protocol::RosterSummaryV1) -> Option<String> {
    s.label.clone()
}

/// Dashboard reports are previews; full stable JSON pages are available via
/// bro_status(detail=report), so arbitrary report.data never grows the list.
fn bro_report_v1_to_dashboard_json(report: &bro_protocol::BroReportV1) -> Value {
    orch::task_report_summary(
        &report.message,
        report.needs.as_deref(),
        report.data.is_some(),
        report.reported_at,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::artifacts;
    use crate::notes::NoteParams;
    use crate::packets::CompileParams;
    use crate::server::state::SharedState;
    use crate::tools::bro_params::{AgentDispatchParams, StatusParams};

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    fn team_params(value: Value) -> TeamParams {
        serde_json::from_value(value).unwrap()
    }

    fn fixture_team(name: &str, project: &str) -> orchestration::team::Team {
        orchestration::team::Team {
            name: name.into(),
            teamplate: "panel".into(),
            project_dir: Some(project.into()),
            created_at: 1,
            members: vec![],
            advisor: None,
            diversity_floor: None,
        }
    }

    #[test]
    fn team_create_receipt_omits_unbounded_member_configuration() {
        let mut team = fixture_team("large", "worker-context");
        team.members = (0..10000)
            .map(|n| orchestration::team::TeamMember {
                name: format!("member-{n}"),
                brofile: "large-lens-reference".repeat(100),
                session_id: None,
                task_history: vec![],
            })
            .collect();
        let receipt = team_create_receipt(&team, true);
        assert_eq!(receipt["memberCount"], 10000);
        assert_eq!(receipt["templateScope"], "global");
        assert!(receipt.get("members").is_none());
        assert!(serde_json::to_vec(&receipt).unwrap().len() < 1000);
        assert!(receipt["detail_hint"].as_str().unwrap().contains("roster"));
    }

    #[tokio::test]
    async fn catalog_team_mutations_refuse_project_sources_and_keep_global_worker_context() {
        use orchestration::{brofile, team};
        let fixture = crate::server::state::catalog_fixture::CatalogFixture::new();
        let server = fixture.server();
        let project = fixture
            .root()
            .canonicalize()
            .unwrap()
            .join("worker-checkout");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.to_str().unwrap();
        let reviewer: brofile::Brofile =
            serde_json::from_value(json!({"name":"reviewer","provider":"glm"})).unwrap();
        brofile::save_brofile(&reviewer, "global", &server.state.store_dir, None).unwrap();
        let mut template = team::Teamplate {
            name: "panel".into(),
            members: vec![team::TeamplateMember {
                brofile: "reviewer".into(),
                alias: None,
                count: 2,
            }],
            advisor: None,
            diversity_floor: None,
        };
        team::save_teamplate(&template, "global", &server.state.store_dir, None).unwrap();
        template.members[0].count = 99;
        team::save_teamplate(&template, "project", &server.state.store_dir, Some(project)).unwrap();
        let path = Path::new(project).join(".bro/teamplates/panel.json");
        let before = std::fs::read(&path).unwrap();
        for action in ["save_template", "delete_template"] {
            let result=server.bro_team(Parameters(team_params(json!({"action":action,"name":"panel","scope":"project","project_dir":project,"members":[{"brofile":"reviewer"}]})))).await;
            assert_eq!(result.is_error, Some(true));
            assert!(extract_text(&result).contains("error.team_template_locality_required"));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let created=server.bro_team(Parameters(team_params(json!({"action":"create","template":"panel","name":"global-team","project_dir":project})))).await;
        assert_ne!(created.is_error, Some(true), "{}", extract_text(&created));
        let receipt: Value = serde_json::from_str(&extract_text(&created)).unwrap();
        assert_eq!(receipt["memberCount"], 2);
        assert_eq!(receipt["templateScope"], "global");
        assert!(receipt.get("members").is_none());
        let saved = team::load_team("global-team", &server.state.store_dir).unwrap();
        assert_eq!(saved.project_dir.as_deref(), Some(project));
        assert_eq!(saved.members.len(), 2);
        assert!(team_source_project_dir(&server, Some(project)).is_none());

        template.name = "project-only".into();
        team::save_teamplate(&template, "project", &server.state.store_dir, Some(project)).unwrap();
        let refused=server.bro_team(Parameters(team_params(json!({"action":"create","template":"project-only","name":"must-not-exist","project_dir":project})))).await;
        assert_eq!(refused.is_error, Some(true));
        assert!(team::load_team("must-not-exist", &server.state.store_dir).is_none());

        let local: brofile::Brofile =
            serde_json::from_value(json!({"name":"project-reviewer","provider":"glm"})).unwrap();
        brofile::save_brofile(&local, "project", &server.state.store_dir, Some(project)).unwrap();
        template.name = "global-local-reference".into();
        template.members[0].brofile = local.name;
        team::save_teamplate(&template, "global", &server.state.store_dir, None).unwrap();
        let refused=server.bro_team(Parameters(team_params(json!({"action":"create","template":"global-local-reference","name":"missing-global-brofile","project_dir":project})))).await;
        assert_eq!(refused.is_error, Some(true));
        assert!(team::load_team("missing-global-brofile", &server.state.store_dir).is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn team_discovery_filters_and_pages_without_expanding_stored_brofiles() {
        use orchestration::team::{self, TeamMember};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let nonexistent = root.join("not-a-checkout").to_string_lossy().into_owned();
        for n in (0..105).rev() {
            let team = fixture_team(
                &format!("team-{n:03}"),
                if n == 104 {
                    "other-project"
                } else {
                    &nonexistent
                },
            );
            team::save_team(&team, &server.state.store_dir);
        }
        let result = server
            .bro_team(Parameters(team_params(
                json!({"action":"list", "project_dir":nonexistent}),
            )))
            .await;
        assert_ne!(result.is_error, Some(true));
        let page: Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(result.structured_content.as_ref(), Some(&page));
        assert_eq!(page["total"], 104);
        assert_eq!(page["count"], 20);
        assert_eq!(page["teams"][0]["name"], "team-000");
        assert_eq!(page["next_offset"], 20);
        let mut params = team_params(
            json!({"action":"list", "project_dir":nonexistent, "offset":100, "limit":1000}),
        );
        let tail = team_discovery(&server, &params).unwrap();
        assert_eq!(tail["limit"], 100);
        assert_eq!(tail["count"], 4);
        assert!(tail["next_offset"].is_null());
        params.name = Some("team-103".into());
        params.offset = None;
        params.limit = Some(0);
        let exact = team_discovery(&server, &params).unwrap();
        assert_eq!(exact["total"], 1);
        assert_eq!(exact["limit"], 1);

        let mut team = fixture_team("members", &nonexistent);
        team.members = (0..105)
            .rev()
            .map(|n| TeamMember {
                name: format!("member-{n:03}"),
                brofile: "uninstalled-brofile".into(),
                session_id: None,
                task_history: vec![format!("old-task-{n}")],
            })
            .collect();
        team::save_team(&team, &server.state.store_dir);
        let roster = team_discovery(
            &server,
            &team_params(json!({"action":"roster","name":"members"})),
        )
        .unwrap();
        assert_eq!(roster["count"], 20);
        assert_eq!(roster["members"][0]["name"], "member-000");
        assert_eq!(roster["members"][0]["latestTask"]["taskId"], "old-task-0");
        assert_eq!(
            roster["members"][0]["latestTask"]["statusUnavailable"],
            true
        );
        assert!(roster["members"][0].get("account").is_none());
        assert!(!Path::new(&nonexistent).exists());
    }

    #[test]
    fn team_exact_body_recovers_oversized_advisor_configuration_and_rejects_stale_cursor() {
        use orchestration::team::{
            self, TeamAdvisor, TeamAdvisorConfig, Teamplate, TeamplateMember,
        };
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let advisor = TeamAdvisorConfig {
            brofile: "advisor".into(),
            alias: None,
            charter: "effective-charter-界".repeat(1000),
            context: Some("retained-context".repeat(1000)),
            halt_conditions: vec!["halt".repeat(1000)],
            exit_conditions: vec![],
            packet_id: None,
            timeout_seconds: None,
            mode: Default::default(),
        };
        let mut template = Teamplate {
            name: "large".into(),
            members: vec![TeamplateMember {
                brofile: "reviewer".into(),
                alias: None,
                count: 1,
            }],
            advisor: Some(advisor.clone()),
            diversity_floor: Some(2),
        };
        team::save_teamplate(&template, "global", &server.state.store_dir, None).unwrap();
        let summary =
            team_discovery(&server, &team_params(json!({"action":"list_templates"}))).unwrap();
        assert_eq!(summary["templates"][0]["memberCount"], 1);
        assert_eq!(summary["templates"][0]["diversityFloor"], 2);
        assert!(!summary.to_string().contains("effective-charter"));
        assert!(!summary.to_string().contains("retained-context"));
        let mut params = team_params(json!({"action":"get_template","name":"large"}));
        let first = team_discovery(&server, &params).unwrap();
        let original_cursor = first["body"]["next_cursor"].as_str().unwrap().to_owned();
        let mut text = String::new();
        loop {
            let page = team_discovery(&server, &params).unwrap();
            assert!(serde_json::to_vec(&page["body"]).unwrap().len() <= 4096);
            text.push_str(page["body"]["text"].as_str().unwrap());
            params.cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if params.cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            serde_json::to_value(&template).unwrap()
        );
        template.members[0].count = 3;
        team::save_teamplate(&template, "global", &server.state.store_dir, None).unwrap();
        params.cursor = Some(original_cursor);
        assert!(
            team_discovery(&server, &params)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        let mut live = fixture_team("live", "stored-association");
        live.advisor = Some(TeamAdvisor {
            name: "advisor".into(),
            config: advisor,
            session_id: None,
            task_history: vec!["retained-task".into()],
        });
        team::save_team(&live, &server.state.store_dir);
        let roster = team_discovery(
            &server,
            &team_params(json!({"action":"roster","name":"live"})),
        )
        .unwrap();
        assert!(!roster.to_string().contains("effective-charter"));
        let exact =
            team_discovery(&server, &team_params(json!({"action":"get","name":"live"}))).unwrap();
        assert!(exact["body"]["next_cursor"].is_string());
    }

    #[test]
    fn team_summary_byte_pages_resume_without_losing_rows() {
        let rows = (0..105)
            .map(|n| json!({"name":format!("member-{n:03}"), "brofile":"界\n".repeat(300)}))
            .collect::<Vec<_>>();
        let mut p = team_params(json!({"action":"roster","name":"large","limit":100}));
        let mut seen = Vec::new();
        loop {
            let page =
                team_summary_page(rows.clone(), "members", &p, json!({"team":"large"})).unwrap();
            assert!(
                serde_json::to_vec(&page).unwrap().len()
                    <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
            );
            for row in page["members"].as_array().unwrap() {
                seen.push(row["name"].as_str().unwrap().to_owned());
            }
            if let Some(next) = page["next_offset"].as_u64() {
                p.offset = Some(next as usize);
            } else {
                break;
            }
        }
        assert_eq!(
            seen,
            (0..105)
                .map(|n| format!("member-{n:03}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn team_discovery_refuses_ambiguous_scope_and_reports_corrupt_records() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        for request in [
            json!({"action":"list_templates","scope":"project"}),
            json!({"action":"list_templates","scope":"typo"}),
            json!({"action":"list_templates","project_dir":"/unused"}),
            json!({"action":"list","scope":"project"}),
            json!({"action":"get","name":"../other"}),
            json!({"action":"roster","name":"team","cursor":"ignored"}),
        ] {
            assert!(validate_team_params(&team_params(request)).is_err());
        }
        let fixture = crate::server::state::catalog_fixture::CatalogFixture::new();
        let catalog_server = fixture.server();
        let result=catalog_server.bro_team(Parameters(team_params(json!({"action":"list_templates","scope":"project","project_dir":"/nonexistent-owner-checkout"})))).await;
        assert_eq!(result.is_error, Some(true));
        assert!(extract_text(&result).contains("error.team_template_locality_required"));
        let dir = server.state.store_dir.join("teamplates");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.json"), "{").unwrap();
        let filtered = team_discovery(
            &server,
            &team_params(json!({"action":"list_templates","name":"absent"})),
        )
        .unwrap();
        assert_eq!(filtered["total"], 0);
        assert!(
            team_discovery(&server, &team_params(json!({"action":"list_templates"})))
                .unwrap_err()
                .to_string()
                .contains("broken")
        );
    }

    #[test]
    fn idle_agent_metrics_omit_zero_and_null_fields() {
        // A single still-running dispatch: only dispatch_count is meaningful.
        let metrics = AgentDashboardMetrics {
            dispatch_count: 1,
            ..AgentDashboardMetrics::default()
        };
        let value = metrics.to_json();
        assert_eq!(value["dispatch_count"], 1);
        for absent in [
            "success_count",
            "failure_count",
            "avg_elapsed_ms",
            "cost_usd_total",
        ] {
            assert!(
                value.get(absent).is_none(),
                "idle agent should omit {absent}: {value}"
            );
        }
    }

    #[test]
    fn active_agent_metrics_emit_populated_fields() {
        // A completed dispatch with cost surfaces every signal-bearing field.
        let metrics = AgentDashboardMetrics {
            dispatch_count: 3,
            success_count: 2,
            failure_count: 1,
            elapsed_ms_total: 6000,
            elapsed_count: 3,
            cost_usd_total: 0.1234,
        };
        let value = metrics.to_json();
        assert_eq!(value["dispatch_count"], 3);
        assert_eq!(value["success_count"], 2);
        assert_eq!(value["failure_count"], 1);
        assert_eq!(value["avg_elapsed_ms"], 2000.0);
        assert_eq!(value["cost_usd_total"], 0.1234);
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn account_mutations_preserve_malformed_or_unreadable_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let path = root.join("config.json");
        let malformed = br#"{"accounts":{"synthetic":{"env":"stored-secret"}}}"#;
        std::fs::write(&path, malformed).unwrap();
        for action in [
            "set_account",
            "set_provider_default",
            "clear_provider_default",
        ] {
            let params: BrofileParams = serde_json::from_value(json!({
                "action": action, "name": "synthetic", "provider": "brodex", "account": "synthetic",
            }))
            .unwrap();
            let result = server.bro_brofile(Parameters(params));
            assert_eq!(
                result.is_error,
                Some(true),
                "{action} overwrote invalid configuration"
            );
            assert!(!extract_text(&result).contains("stored-secret"));
            assert_eq!(std::fs::read(&path).unwrap(), malformed);
        }
        // A directory cannot be read as JSON, regardless of test permissions.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("retained"), "existing-data").unwrap();
        let params: BrofileParams =
            serde_json::from_value(json!({"action": "set_account", "name": "synthetic"})).unwrap();
        let result = server.bro_brofile(Parameters(params));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            std::fs::read_to_string(path.join("retained")).unwrap(),
            "existing-data"
        );
    }

    #[test]
    fn account_env_update_preserves_existing_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let mut config = orchestration::brofile::BroConfig::default();
        config.accounts.insert(
            "synthetic".into(),
            orchestration::brofile::Account {
                disabled: true,
                allowed_models: vec!["test-model".into()],
                allowed_tiers: vec!["test-tier".into()],
                max_concurrent: Some(2),
                ..Default::default()
            },
        );
        orchestration::brofile::save_config(&config, &server.state.store_dir).unwrap();
        let params: BrofileParams = serde_json::from_value(json!({
            "action": "set_account", "name": "synthetic", "env": {"TOKEN": "new-secret"},
        }))
        .unwrap();
        let result = server.bro_brofile(Parameters(params));
        assert_ne!(result.is_error, Some(true));
        let updated =
            orchestration::brofile::load_account("synthetic", &server.state.store_dir).unwrap();
        assert!(updated.disabled);
        assert_eq!(updated.allowed_models, ["test-model"]);
        assert_eq!(updated.allowed_tiers, ["test-tier"]);
        assert_eq!(updated.max_concurrent, Some(2));
        assert_eq!(updated.env.unwrap()["TOKEN"], "new-secret");
    }

    #[test]
    fn account_mutations_report_failed_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut state = SharedState::for_test(&root);
        // A file where the store directory should be fails deterministically,
        // including tests running as root, without changing filesystem modes.
        let blocked = root.join("blocked-store");
        std::fs::write(&blocked, "existing-file").unwrap();
        state.store_dir = blocked.clone();
        let server = BlackboxServer::new(Arc::new(state));
        for action in [
            "set_account",
            "set_provider_default",
            "clear_provider_default",
        ] {
            let params: BrofileParams = serde_json::from_value(json!({
                "action": action, "name": "synthetic", "provider": "brodex",
                "account": "synthetic", "env": {"TOKEN": "synthetic-secret"},
            }))
            .unwrap();
            let result = server.bro_brofile(Parameters(params));
            assert_eq!(result.is_error, Some(true), "{action} claimed success");
            let response = extract_text(&result);
            assert!(response.contains("not saved"));
            assert!(!response.contains("synthetic-secret"));
            assert!(!response.contains("\"updated\": true"));
        }
        assert_eq!(std::fs::read_to_string(blocked).unwrap(), "existing-file");
    }

    #[test]
    fn account_responses_hide_environment_values_but_persist_them() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let params: BrofileParams = serde_json::from_value(json!({
            "action": "set_account", "name": "synthetic",
            "env": {"TOKEN": "synthetic-secret", "CUSTOM": "opaque-secret"},
        }))
        .unwrap();
        let result = server.bro_brofile(Parameters(params));
        assert_ne!(result.is_error, Some(true));
        let set_reply = extract_text(&result);
        let params: BrofileParams =
            serde_json::from_value(json!({"action": "list_accounts"})).unwrap();
        let list_reply = extract_text(&server.bro_brofile(Parameters(params)));
        for reply in [&set_reply, &list_reply] {
            assert!(!reply.contains("synthetic-secret"));
            assert!(!reply.contains("opaque-secret"));
            assert!(reply.contains("TOKEN"));
            assert!(reply.contains("CUSTOM"));
        }
        let list: Value = serde_json::from_str(&list_reply).unwrap();
        assert_eq!(list["synthetic"]["env_keys"], json!(["CUSTOM", "TOKEN"]));
        let account =
            orchestration::brofile::load_account("synthetic", &server.state.store_dir).unwrap();
        assert_eq!(account.env.unwrap()["TOKEN"], "synthetic-secret");
    }

    #[test]
    fn providers_expand_only_the_selected_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let summary = server.bro_providers(Parameters(ProvidersParams { provider: None }));
        let summary: Value = serde_json::from_str(&extract_text(&summary)).unwrap();
        assert_eq!(summary.as_object().unwrap().len(), Provider::ALL.len());
        for entry in summary.as_object().unwrap().values() {
            for local_only in ["bin", "found", "path"] {
                assert!(entry.get(local_only).is_none());
            }
            assert!(entry.get("models").is_none());
            assert!(entry.get("efforts").is_none());
        }
        let detail = server.bro_providers(Parameters(ProvidersParams {
            provider: Some("brodex".into()),
        }));
        let detail: Value = serde_json::from_str(&extract_text(&detail)).unwrap();
        assert_eq!(detail.as_object().unwrap().len(), 1);
        assert!(
            detail["brodex"]["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "gpt-6-astra")
        );
        assert_eq!(detail["brodex"]["defaultModel"], "gpt-5.6-sol");
        for local_only in ["bin", "found", "path"] {
            assert!(detail["brodex"].get(local_only).is_none());
        }
        let workflow = server.bro_providers(Parameters(ProvidersParams {
            provider: Some("workflow".into()),
        }));
        assert_eq!(workflow.is_error, Some(true));
        let invalid = server.bro_providers(Parameters(ProvidersParams {
            provider: Some("typo".into()),
        }));
        assert_eq!(invalid.is_error, Some(true));
    }

    // Wave 7c: bro_dashboard now reads from the materialized
    // RosterView. These tests seed the view (via the sink or via
    // `rebuild_from_store`, matching the wave-6a convention) and
    // assert the row projection field-for-field against the legacy
    // wire shape. The dashboard row MUST stay byte-compatible for
    // existing consumers (the wave-7c invariant).
    mod dashboard_view {
        use super::*;
        use bro_core::{Origin, SessionId, TaskId};
        use bro_protocol::TaskStatus as WireTaskStatus;
        use bro_protocol::{BroReportV1, RosterSummaryV1};

        fn live_summary(id: &str, provider: Provider, started_at: u64) -> RosterSummaryV1 {
            RosterSummaryV1 {
                task_id: TaskId::new(id),
                status: WireTaskStatus::Running,
                provider,
                cost: Some(0.10),
                turns: Some(4),
                cwd: Some("/work/alpha".to_string()),
                label: Some("team::executor".to_string()),
                name: Some("Inspect the failing roster columns".to_string()),
                session_id: Some(SessionId::new(format!("sess-{id}"))),
                last_message_snippet: Some("hello".to_string()),
                model: Some("glm-pro".to_string()),
                report: Some("teaser".to_string()),
                last_event_at: Some(started_at),
                origin: Origin::Cockpit,
                managed_worktree: Some("/wt/alpha".to_string()),
                workflow_owned: false,
                started_at: Some(started_at),
                agent_label: Some(format!("agent-{id}@v1")),
                report_full: Some(BroReportV1 {
                    message: "writing focused tests".to_string(),
                    needs: Some("review API naming".to_string()),
                    data: None,
                    reported_at: started_at,
                    reported_ago: "0s".to_string(),
                }),
                interrupted: false,
                error_teaser: None,
                transcript_path: None,
                context: None,
            }
        }

        fn terminal_summary(
            id: &str,
            provider: Provider,
            started_at: u64,
            completed_at: u64,
        ) -> RosterSummaryV1 {
            RosterSummaryV1 {
                task_id: TaskId::new(id),
                status: WireTaskStatus::Completed,
                provider,
                cost: Some(0.42),
                turns: Some(7),
                cwd: None,
                label: Some("team::reviewer".to_string()),
                name: Some(format!("Prompt teaser {id}")),
                session_id: Some(SessionId::new(format!("sess-{id}"))),
                last_message_snippet: None,
                model: None,
                report: None,
                last_event_at: Some(completed_at),
                origin: Origin::AgentDispatch,
                managed_worktree: None,
                workflow_owned: false,
                started_at: Some(started_at),
                agent_label: Some(format!("agent-{id}@v1")),
                report_full: None,
                interrupted: false,
                error_teaser: None,
                transcript_path: None,
                context: None,
            }
        }

        #[test]
        fn dashboard_row_matches_legacy_shape_for_live_and_terminal_tasks() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);

            // Seed the view directly with two summaries that
            // exercise the live (Running) and terminal (Completed)
            // paths the dashboard projection branches on. The
            // started_at / completed_at timestamps are pinned so
            // the recomputed `elapsed` field is stable across
            // wall-clock drift during the test run.
            let live_start = 1_700_000_000_000_u64;
            let terminal_start = 1_700_000_010_000_u64;
            let terminal_done = 1_700_000_011_500_u64;
            server.state.roster_view.upsert(
                "live-1".to_string(),
                live_summary("live-1", Provider::Glm, live_start),
            );
            server.state.roster_view.upsert(
                "term-1".to_string(),
                terminal_summary("term-1", Provider::Deepseek, terminal_start, terminal_done),
            );

            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: None,
                team: None,
            }));
            assert_ne!(dash.is_error, Some(true));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().expect("tasks must be array");
            assert_eq!(tasks.len(), 2, "both seeded tasks should appear");

            // Sort-agnostic lookup by task_id.
            let by_id: std::collections::HashMap<_, _> = tasks
                .iter()
                .map(|t| (t["taskId"].as_str().unwrap().to_string(), t.clone()))
                .collect();

            // Live task: provider / status serialize in the same
            // wire form the legacy projection emitted (lowercase
            // variant); elapsed is `now - started_at` (a live
            // string like "5s", recomputed at call time).
            let live = by_id.get("live-1").expect("live-1 row present");
            assert_eq!(live["provider"], "glm");
            assert_eq!(live["status"], "running");
            assert_eq!(live["sessionId"], "sess-live-1");
            assert!(
                !live["hasResult"].as_bool().unwrap_or(true),
                "live task must not report hasResult=true"
            );
            assert!(
                live["hasLastMessage"].as_bool().unwrap_or(false),
                "live task with snippet should have hasLastMessage=true"
            );
            assert_eq!(live["broLabel"], "team::executor");
            assert_eq!(live["agentLabel"], "agent-live-1@v1");
            assert_eq!(live["report"]["message"], "writing focused tests");
            assert_eq!(live["report"]["needs"], "review API naming");
            // `elapsed` is a live display; just check it parses as
            // "<n>s" or "<n>m <n>s" — anything else is a regression
            // in `format_elapsed` rather than the dashboard.
            let live_elapsed = live["elapsed"].as_str().unwrap();
            assert!(
                live_elapsed.ends_with('s') && !live_elapsed.is_empty(),
                "live elapsed shape regressed: {live_elapsed}"
            );

            // Terminal task: status is `completed`, elapsed is
            // `completed_at - started_at` = 1500ms = "1s", and
            // `hasResult` is false (no last_message_snippet).
            // `hasLastMessage` is also false (no snippet at all).
            let term = by_id.get("term-1").expect("term-1 row present");
            assert_eq!(term["provider"], "deepseek");
            assert_eq!(term["status"], "completed");
            assert_eq!(term["sessionId"], "sess-term-1");
            assert!(!term["hasResult"].as_bool().unwrap_or(true));
            assert!(!term["hasLastMessage"].as_bool().unwrap_or(true));
            assert_eq!(term["broLabel"], "team::reviewer");
            assert_eq!(term["agentLabel"], "agent-term-1@v1");
            assert_eq!(term["elapsed"], "1s");
            // Terminal task has no report in the seed.
            assert!(term.get("report").is_none() || term["report"].is_null());

            // Agents rollup: only the tasks that carry an
            // `agent_label` show up in the agents map. Each seeded
            // task is one dispatch for its agent label, but they
            // share labels across `live-1` and `term-1`? No — each
            // label is unique per seeded summary, so we expect two
            // distinct entries with `dispatch_count: 1` each.
            let agents = body["agents"].as_object().expect("agents must be object");
            assert_eq!(agents.len(), 2);
            assert_eq!(agents["agent-live-1@v1"]["dispatch_count"], 1);
            assert_eq!(agents["agent-term-1@v1"]["dispatch_count"], 1);
            // The terminal dispatch landed a success_count because
            // status is `completed`; the live one has no
            // success/failure tally yet.
            assert_eq!(
                agents["agent-term-1@v1"]["success_count"], 1,
                "terminal success must roll up: {agents:?}"
            );
            assert_eq!(agents["agent-live-1@v1"]["success_count"].as_u64(), None);
        }

        #[test]
        fn dashboard_invalid_filters_fail_without_broadening() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            server.state.roster_view.upsert(
                "unrelated-task".into(),
                live_summary("unrelated-task", Provider::Brodex, 1_700_000_000_000),
            );
            for input in [
                json!({"provider": "typo"}),
                json!({"provider": ""}),
                json!({"status": "typo"}),
                json!({"status": ""}),
                json!({"team": "missing-team"}),
                json!({"team": ""}),
            ] {
                let params: DashboardParams = serde_json::from_value(input.clone()).unwrap();
                let result = server.bro_dashboard(Parameters(params));
                assert_eq!(result.is_error, Some(true), "filter broadened: {input}");
                assert!(!extract_text(&result).contains("unrelated-task"));
            }
            let params: DashboardParams =
                serde_json::from_value(json!({"provider": "kimi"})).unwrap();
            let result = server.bro_dashboard(Parameters(params));
            assert_ne!(result.is_error, Some(true));
            let body: Value = serde_json::from_str(&extract_text(&result)).unwrap();
            assert_eq!(body["tasks"], json!([]));
        }

        #[test]
        fn dashboard_pagination_bounds_agent_rollup_and_report_payload() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            for idx in 0..4 {
                let id = format!("task-{idx}");
                let mut summary = live_summary(&id, Provider::Glm, 1000 + idx);
                summary.report_full.as_mut().unwrap().data =
                    Some(json!({"trace": "x".repeat(40000)}));
                server.state.roster_view.upsert(id, summary);
            }
            let read_page = |offset| {
                let response = server.bro_dashboard(Parameters(DashboardParams {
                    provider: None,
                    team: None,
                    status: None,
                    limit: Some(1),
                    offset: Some(offset),
                }));
                serde_json::from_str::<Value>(&extract_text(&response)).unwrap()
            };
            let first = read_page(0);
            let second = read_page(first["next_offset"].as_u64().unwrap() as usize);
            assert_eq!(first["total"], 4);
            assert_eq!(first["tasks"][0]["taskId"], "task-3");
            assert_eq!(second["tasks"][0]["taskId"], "task-2");
            assert_eq!(first["agents"].as_object().unwrap().len(), 1);
            assert_eq!(second["agents"].as_object().unwrap().len(), 1);
            assert!(first["tasks"][0]["report"].get("data").is_none());
            assert_eq!(first["tasks"][0]["report"]["detailsOmitted"], true);
            assert!(serde_json::to_vec(&first).unwrap().len() < 4096);
        }

        #[test]
        fn dashboard_invalid_filters_do_not_broaden_the_selection() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            for (provider, status, team) in [
                (Some("unknown"), None, None),
                (None, Some("done"), None),
                (None, None, Some("missing-team")),
            ] {
                let response = server.bro_dashboard(Parameters(DashboardParams {
                    provider: provider.map(str::to_string),
                    status: status.map(str::to_string),
                    team: team.map(str::to_string),
                    limit: None,
                    offset: None,
                }));
                assert_eq!(response.is_error, Some(true));
            }
        }

        #[test]
        fn dashboard_filter_by_status_and_provider_runs_against_view() {
            // Filters must apply on the snapshot, not the
            // per-task inner lock. Seed one live and one terminal
            // task across two providers and assert each filter
            // returns the expected subset.
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            let t = 1_700_000_000_000_u64;
            server
                .state
                .roster_view
                .upsert("a".to_string(), live_summary("a", Provider::Glm, t));
            server.state.roster_view.upsert(
                "b".to_string(),
                terminal_summary("b", Provider::Deepseek, t, t + 1000),
            );
            server.state.roster_view.upsert(
                "c".to_string(),
                terminal_summary("c", Provider::Glm, t, t + 2000),
            );

            // status="running" → only `a`.
            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: Some("running".into()),
                team: None,
            }));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 1, "running filter should leave one row");
            assert_eq!(tasks[0]["taskId"], "a");

            // provider="deepseek" → only `b`.
            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: Some("deepseek".into()),
                status: None,
                team: None,
            }));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0]["taskId"], "b");

            // provider="glm" + status="completed" → only `c`.
            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: Some("glm".into()),
                status: Some("completed".into()),
                team: None,
            }));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0]["taskId"], "c");
        }

        #[test]
        fn dashboard_surfaces_interrupted_cancelled_rows() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            let t = 1_700_000_000_000_u64;
            let mut interrupted = terminal_summary("interrupted", Provider::Brodex, t, t + 1000);
            interrupted.status = WireTaskStatus::Cancelled;
            interrupted.interrupted = true;
            interrupted.last_message_snippet = Some("partial output".to_string());
            server
                .state
                .roster_view
                .upsert("interrupted".to_string(), interrupted);

            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: Some("cancelled".into()),
                team: None,
            }));
            assert_ne!(dash.is_error, Some(true));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().expect("tasks must be array");
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0]["taskId"], "interrupted");
            assert_eq!(tasks[0]["status"], "cancelled");
            assert_eq!(tasks[0]["interrupted"], true);
            assert_eq!(tasks[0]["hasResult"], true);
        }

        #[test]
        fn dashboard_row_carries_context_pressure_when_present() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            let t = 1_700_000_000_000_u64;

            let mut pressured = live_summary("pressured", Provider::Glm, t);
            pressured.context = Some(bro_protocol::ContextPressure::derive(
                190_000,
                Some(200_000),
                0.8,
            ));
            server
                .state
                .roster_view
                .upsert("pressured".to_string(), pressured);
            // A second row with no measurement must stay silent rather than
            // report a zero that reads as an empty window.
            server.state.roster_view.upsert(
                "quiet".to_string(),
                live_summary("quiet", Provider::Glm, t - 1000),
            );

            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: None,
                team: None,
            }));
            assert_ne!(dash.is_error, Some(true));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let by_id: std::collections::HashMap<_, _> = body["tasks"]
                .as_array()
                .expect("tasks must be array")
                .iter()
                .map(|t| (t["taskId"].as_str().unwrap().to_string(), t.clone()))
                .collect();

            let row = by_id.get("pressured").expect("pressured row present");
            assert_eq!(row["context"]["last_turn_input_tokens"], 190_000);
            assert_eq!(row["context"]["context_window"], 200_000);
            assert_eq!(row["context"]["utilization"], 0.95);
            assert!(row["context"].get("approaching_ceiling").is_none());
            assert_eq!(row["context"]["measurement"], "last_model_request");
            assert!(row["context"].get("guidance").is_none());
            assert!(body["context_hint"].as_str().unwrap().contains("budget"));

            let quiet = by_id.get("quiet").expect("quiet row present");
            assert!(
                quiet.get("context").is_none(),
                "a row with no measurement must omit the block entirely: {quiet}"
            );
        }

        #[test]
        fn dashboard_sort_order_is_started_at_descending() {
            // The legacy sort key was `started_at` DESC. With the
            // view snapshot, the order is non-deterministic, so
            // the dashboard must still sort by `started_at` (or
            // `last_event_at` fallback) DESC. Seed three tasks
            // with explicit started_at and verify the served order.
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            server.state.roster_view.upsert(
                "old".to_string(),
                terminal_summary("old", Provider::Glm, 1_000, 2_000),
            );
            server
                .state
                .roster_view
                .upsert("new".to_string(), live_summary("new", Provider::Glm, 9_000));
            server.state.roster_view.upsert(
                "mid".to_string(),
                terminal_summary("mid", Provider::Glm, 5_000, 6_000),
            );

            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: None,
                team: None,
            }));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().unwrap();
            let order: Vec<&str> = tasks
                .iter()
                .map(|t| t["taskId"].as_str().unwrap())
                .collect();
            assert_eq!(
                order,
                vec!["new", "mid", "old"],
                "dashboard must sort by started_at DESC"
            );
        }

        #[test]
        fn dashboard_reads_from_seeded_view_not_per_task_lock() {
            // RosterView is the dashboard's read path; the handler
            // MUST NOT lock any per-task inner mutex. Seed a
            // summary directly (no inner mutex involved) and
            // assert the row appears. If the dashboard fell back
            // to `task_store.all_tasks()`, this test would
            // produce an empty body.
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            let t = 1_700_000_000_000_u64;
            server.state.roster_view.upsert(
                "view-only".to_string(),
                live_summary("view-only", Provider::Glm, t),
            );

            // Sanity: no task is in the store — only the view.
            assert!(server.state.task_store.read().all_tasks().is_empty());

            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: None,
                team: None,
            }));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().unwrap();
            assert_eq!(
                tasks.len(),
                1,
                "dashboard must serve from the view, not the task store"
            );
            assert_eq!(tasks[0]["taskId"], "view-only");
        }

        #[test]
        fn dashboard_rebuild_from_store_seeds_view_for_dashboard_path() {
            // The same cold-start pattern the wave-6a
            // /control/roster tests pinned: insert into the
            // store, call rebuild_from_store, then read the
            // dashboard. The handler must not need the per-task
            // inner lock to project rows.
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            let t_live = 1_700_000_000_000_u64;
            let t_done = 1_700_000_010_000_u64;
            {
                let mut store = server.state.task_store.write();
                let live = orchestration::test_task(
                    "live-1",
                    orchestration::TaskStatus::Running,
                    Provider::Glm,
                );
                {
                    let mut inner = live.inner.lock();
                    inner.started_at = t_live;
                    inner.completed_at = None;
                    inner.bro_label = Some("team::executor".into());
                    inner.agent_label = Some("agent-live-1@v1".into());
                    inner.last_assistant_message = Some("hi".into());
                    inner.session_id = "sess-live-1".into();
                }
                store.insert("live-1".into(), live).expect("insert live-1");

                let done = orchestration::test_task(
                    "done-1",
                    orchestration::TaskStatus::Completed,
                    Provider::Deepseek,
                );
                {
                    let mut inner = done.inner.lock();
                    inner.started_at = t_done;
                    inner.completed_at = Some(t_done + 1_500);
                    inner.cost_usd = Some(0.5);
                    inner.num_turns = Some(2);
                    inner.bro_label = Some("team::reviewer".into());
                    inner.agent_label = Some("agent-done-1@v1".into());
                }
                store.insert("done-1".into(), done).expect("insert done-1");
            }
            server
                .state
                .roster_view
                .rebuild_from_store(&server.state.task_store.read());

            let dash = server.bro_dashboard(Parameters(DashboardParams {
                offset: None,
                limit: Some(20),
                provider: None,
                status: None,
                team: None,
            }));
            let body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
            let tasks = body["tasks"].as_array().unwrap();
            assert_eq!(tasks.len(), 2);

            let by_id: std::collections::HashMap<_, _> = tasks
                .iter()
                .map(|t| (t["taskId"].as_str().unwrap().to_string(), t.clone()))
                .collect();

            let live = by_id.get("live-1").expect("live-1 row");
            assert_eq!(live["provider"], "glm");
            assert_eq!(live["status"], "running");
            assert_eq!(live["broLabel"], "team::executor");
            assert_eq!(live["agentLabel"], "agent-live-1@v1");
            assert!(
                !live["hasResult"].as_bool().unwrap_or(true),
                "live task must not report hasResult"
            );
            assert!(
                live["hasLastMessage"].as_bool().unwrap_or(false),
                "live task with message should have hasLastMessage"
            );

            let done = by_id.get("done-1").expect("done-1 row");
            assert_eq!(done["provider"], "deepseek");
            assert_eq!(done["status"], "completed");
            assert_eq!(done["broLabel"], "team::reviewer");
            assert_eq!(done["agentLabel"], "agent-done-1@v1");
            assert_eq!(done["elapsed"], "1s");
        }
    }

    #[test]
    fn build_team_advisor_init_prompt_includes_charter_halt_exit_and_status_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let team = orchestration::team::Team {
            name: "migration-team".into(),
            teamplate: "tp".into(),
            members: vec![
                orchestration::team::TeamMember {
                    name: "executor".into(),
                    brofile: "codex-exec".into(),
                    session_id: None,
                    task_history: vec![],
                },
                orchestration::team::TeamMember {
                    name: "reviewer".into(),
                    brofile: "claude-review".into(),
                    session_id: None,
                    task_history: vec![],
                },
            ],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        let advisor = orchestration::team::TeamAdvisor {
            name: "lead-advisor".into(),
            config: orchestration::team::TeamAdvisorConfig {
                brofile: "advisor-brofile".into(),
                alias: Some("lead-advisor".into()),
                charter: "keep the migration honest; reject fake phase boundaries".into(),
                context: Some("phase 2 of 3".into()),
                halt_conditions: vec![
                    "executor invents a phase boundary that masks coupling".into(),
                    "reviewer rubber-stamps a phase without adversarial read".into(),
                ],
                exit_conditions: vec!["all three phases land and are reviewed".into()],
                packet_id: Some("packet-abcdef12".into()),
                timeout_seconds: None,
                mode: orchestration::team::AdvisorMode::Blocking,
            },
            session_id: None,
            task_history: vec![],
        };

        let prompt = server.build_team_advisor_init_prompt(&team, &advisor);

        // Status schema — load-bearing for orchestrator parsing of advisor output.
        assert!(
            prompt.contains("Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO"),
            "advisor init prompt missing canonical status schema: {prompt}"
        );
        assert!(prompt.contains("Rationale:"), "missing Rationale line");
        assert!(prompt.contains("Next step:"), "missing Next step line");

        // Charter, context, packet_id round-tripped verbatim.
        assert!(prompt.contains("keep the migration honest"));
        assert!(prompt.contains("phase 2 of 3"));
        assert!(prompt.contains("packet-abcdef12"));

        // Every halt and exit condition must survive as its own bullet.
        assert!(prompt.contains("- executor invents a phase boundary that masks coupling"));
        assert!(prompt.contains("- reviewer rubber-stamps a phase without adversarial read"));
        assert!(prompt.contains("- all three phases land and are reviewed"));

        // Team roster surfaces so the advisor knows who it is steering.
        assert!(prompt.contains("executor (codex-exec)"));
        assert!(prompt.contains("reviewer (claude-review)"));
        assert!(prompt.contains("migration-team"));
    }

    #[test]
    fn advisor_checkpoint_serializes_with_packet_entity_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let team = orchestration::team::Team {
            name: "demo".into(),
            teamplate: "tp".into(),
            members: vec![],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        let checkpoint = server.build_advisor_checkpoint(
            &team,
            "wait",
            &[
                json!({
                    "taskId": "task-a",
                    "status": "completed",
                    "bro": "exec",
                    "result": "ok"
                }),
                json!({
                    "taskId": "task-b",
                    "status": "running",
                    "bro": "reviewer",
                    "timed_out": true
                }),
            ],
        );
        let serialized = serde_json::to_value(&checkpoint).unwrap();

        // Fields the packet evaluator uses as predicate operands. If any of
        // these drift, every advisor packet in the wild breaks silently.
        for key in [
            "wait_kind",
            "team_name",
            "total_count",
            "completed_count",
            "failed_count",
            "running_count",
            "timed_out_count",
            "blocked_count",
            "dispute_count",
            "done_count",
            "members",
            "notes",
        ] {
            assert!(
                serialized.get(key).is_some(),
                "advisor checkpoint missing packet-facing field '{key}': {serialized}"
            );
        }

        assert_eq!(serialized["total_count"], 2);
        assert_eq!(serialized["completed_count"], 1);
        assert_eq!(serialized["running_count"], 1);
        assert_eq!(serialized["timed_out_count"], 1);
    }

    #[test]
    fn apply_advisor_packet_returns_rule_hit_for_checkpoint_entity() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let packet_id = {
            let store = server.state.packets.read();
            let result = store
                .compile(&CompileParams {
                    domain: "advisor/demo-escalate".into(),
                    classification_lattice: Some(vec!["escalate".into(), "continue".into()]),
                    prefix_inference: Some(
                        [
                            ("escalate_".into(), "escalate".into()),
                            ("continue_".into(), "continue".into()),
                        ]
                        .into(),
                    ),
                    rules: json!([
                        {
                            "id": "escalate_any_blocked",
                            "antecedent": {"op": "Gt", "field": "blocked_count", "value": 0},
                            "consequent": "ESCALATE"
                        },
                        {
                            "id": "continue_default",
                            "classification": "continue",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "CONTINUE"
                        }
                    ]),
                    scope: Some("global".into()),
                    project: None,
                    project_id: None,
                    source_ids: None,
                    rank_table: None,
                    rank_lookup_key: None,
                    threshold_table: None,
                    threshold_lookup_key: None,
                })
                .unwrap();
            // compile() returns "Packet packet-<id> compiled (...)" — extract id.
            result
                .split_whitespace()
                .find(|tok| tok.starts_with("packet-"))
                .unwrap()
                .to_string()
        };

        let team = orchestration::team::Team {
            name: "t".into(),
            teamplate: "tp".into(),
            members: vec![],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        {
            let mut notes = server.state.notes.write();
            notes
                .create(&NoteParams {
                    kind: "blocked".into(),
                    body: "exec is stuck".into(),
                    task_id: Some("task-x".into()),
                    session_id: None,
                    project: None,
                    project_id: None,
                    thread_id: None,
                    provider: None,
                    bro: Some("exec".into()),
                })
                .unwrap();
        }
        let checkpoint = server.build_advisor_checkpoint(
            &team,
            "wait",
            &[json!({"taskId": "task-x", "status": "running"})],
        );

        let verdict = server
            .apply_advisor_packet(&packet_id, &checkpoint)
            .unwrap();
        assert_eq!(verdict["match"], true);
        assert_eq!(verdict["ruleId"], "escalate_any_blocked");
        assert_eq!(verdict["classification"], "escalate");
    }

    #[test]
    fn build_advisor_checkpoint_flattens_note_counts_for_packets() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        {
            let mut notes = server.state.notes.write();
            notes
                .create(&NoteParams {
                    kind: "blocked".into(),
                    body: "worker is blocked".into(),
                    task_id: Some("task-1".into()),
                    session_id: None,
                    project: None,
                    project_id: None,
                    thread_id: None,
                    provider: None,
                    bro: Some("worker".into()),
                })
                .unwrap();
            notes
                .create(&NoteParams {
                    kind: "dispute".into(),
                    body: "worker disputes premise".into(),
                    task_id: Some("task-1".into()),
                    session_id: None,
                    project: None,
                    project_id: None,
                    thread_id: None,
                    provider: None,
                    bro: Some("worker".into()),
                })
                .unwrap();
        }
        let team = orchestration::team::Team {
            name: "demo".into(),
            teamplate: "tp".into(),
            members: vec![],
            advisor: Some(orchestration::team::TeamAdvisor {
                name: "advisor".into(),
                config: orchestration::team::TeamAdvisorConfig {
                    brofile: "advisor".into(),
                    alias: Some("advisor".into()),
                    charter: "demo".into(),
                    context: None,
                    halt_conditions: vec![],
                    exit_conditions: vec![],
                    packet_id: Some("packet-demo".into()),
                    timeout_seconds: None,
                    mode: orchestration::team::AdvisorMode::Blocking,
                },
                session_id: None,
                task_history: vec![],
            }),
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        let checkpoint = server.build_advisor_checkpoint(
            &team,
            "when_all",
            &[json!({
                "taskId": "task-1",
                "status": "running",
                "timed_out": true
            })],
        );
        assert_eq!(checkpoint.blocked_count, 1);
        assert_eq!(checkpoint.dispute_count, 1);
        assert_eq!(checkpoint.notes.blocked_count, 1);
        assert_eq!(checkpoint.notes.dispute_count, 1);
    }
    // Real agent dispatch in-process: contends on shared dispatch/provider
    // state under the full parallel suite (flaky). Opt-in via `--ignored`.
    #[test]
    #[ignore = "real agent dispatch; run with --ignored"]
    fn bro_dashboard_emits_agent_label() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "dash-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "dash-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent for dashboard test.",
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
            agent: "dash-agent".into(),
            args: serde_json::Value::Null,
            cwd: Some(tmp.path().to_str().unwrap().to_string()),
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = body["task_id"].as_str().unwrap();

        let dash = server.bro_dashboard(Parameters(DashboardParams {
            offset: None,
            limit: Some(20),
            provider: None,
            status: None,
            team: None,
        }));
        let dash_body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
        let tasks = dash_body["tasks"].as_array().unwrap();
        let found = tasks.iter().find(|t| t["taskId"].as_str() == Some(task_id));
        assert!(found.is_some(), "task should appear in dashboard");
        let entry = found.unwrap();
        assert_eq!(
            entry["agentLabel"].as_str(),
            Some("agent:dash-agent@v1"),
            "dashboard entry should carry agentLabel: {entry}"
        );
        assert_eq!(
            entry["broLabel"].as_str(),
            Some("agent:dash-agent@v1"),
            "dashboard entry should carry broLabel: {entry}"
        );
        let agent_metrics = &dash_body["agents"]["agent:dash-agent@v1"];
        assert_eq!(agent_metrics["dispatch_count"].as_u64(), Some(1));
        assert_eq!(agent_metrics["success_count"].as_u64(), Some(0));
        assert_eq!(agent_metrics["failure_count"].as_u64(), Some(0));
    }

    // Real agent dispatch in-process (same opt-in rationale as above).
    #[test]
    #[ignore = "real agent dispatch; run with --ignored"]
    fn bro_report_surfaces_latest_task_report() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = &server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "report-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "report-agent",
                "version": 1,
                "manifest": {
                    "description": "Agent for report test.",
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
            agent: "report-agent".into(),
            args: serde_json::Value::Null,
            cwd: Some(tmp.path().to_str().unwrap().to_string()),
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
            runtime: None,
        })));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = body["task_id"].as_str().unwrap().to_string();

        let report = server.bro_report(Parameters(ReportParams {
            task_id: task_id.clone(),
            message: "writing focused tests".into(),
            needs: Some("review API naming".into()),
            data: Some(serde_json::json!({"phase": "test"})),
        }));
        assert_ne!(report.is_error, Some(true));
        let report_body: serde_json::Value = serde_json::from_str(&extract_text(&report)).unwrap();
        assert_eq!(
            report_body["report"]["message"].as_str(),
            Some("writing focused tests")
        );
        assert_eq!(
            report_body["report"]["needs"].as_str(),
            Some("review API naming")
        );
        assert_eq!(
            report_body["report"]["data"]["phase"].as_str(),
            Some("test")
        );
        assert!(report_body["report"]["reportedAt"].as_u64().is_some());
        assert!(report_body["report"]["reportedAgo"].as_str().is_some());

        let dash = server.bro_dashboard(Parameters(DashboardParams {
            offset: None,
            limit: Some(20),
            provider: None,
            status: None,
            team: None,
        }));
        let dash_body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
        let entry = dash_body["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["taskId"].as_str() == Some(task_id.as_str()))
            .expect("task should appear in dashboard");
        assert_eq!(
            entry["report"]["message"].as_str(),
            Some("writing focused tests")
        );
        assert_eq!(entry["report"]["needs"].as_str(), Some("review API naming"));

        let status = server.bro_status(Parameters(StatusParams {
            detail: None,
            cursor: None,
            limit: None,
            debug: false,
            task_id: task_id.clone(),
            tail: None,
        }));
        let status_body: serde_json::Value = serde_json::from_str(&extract_text(&status)).unwrap();
        assert_eq!(
            status_body["report"]["message"].as_str(),
            Some("writing focused tests")
        );
    }
}
