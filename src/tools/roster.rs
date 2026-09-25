use std::path::Path;
use std::sync::Arc;

use crate::orchestration;
use crate::orchestration as orch;
use crate::orchestration::brofile::store_identity;
use crate::orchestration::providers::Provider;
use crate::server::progress::extra_filters_from_params;
use crate::server::state::BlackboxServer;
use crate::tools::bro_params::{BrofileParams, DashboardParams, ProvidersParams};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::roster_tools()
}

/// Bounded preview of a free-text diagnostic field. Not a redaction: this
/// only bounds transport size and marks truncation.
fn preview_text(text: &str, max_bytes: usize) -> serde_json::Value {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == text.len() {
        return serde_json::Value::String(text.to_string());
    }
    serde_json::json!({
        "text": &text[..end],
        "text_truncated": true,
        "total_bytes": text.len(),
    })
}

#[tool_router(router = roster_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_dashboard",
        description = "Page recent task summaries for lookup; do not take over another operator's task."
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

        // Wave 7c: read the materialized RosterView snapshot instead
        // of iterating `task_store` and locking every per-task inner
        // mutex. `RosterEventSink::emit_*` keeps the view fresh at
        // the same call sites that touch `TaskInner`, so a snapshot
        // serves the same fields the legacy projection read under
        // the lock — without contending with event ingest on busy
        // tasks (invariant I6 of design/daemon-runtime/concurrency-
        // model.md).
        let snapshot = self.state.roster_view.snapshot();

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
        let entries: Vec<Value> = selected
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|s| {
                let task_id_str = s.task_id.as_str().to_string();

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

                let has_last_message = s
                    .has_last_message
                    .unwrap_or(s.last_message_snippet.is_some());
                let session_id_str = s.session_id.as_ref().map(|s| s.as_str().to_string());
                let mut entry = json!({
                    "taskId": task_id_str,
                    "provider": s.provider,
                    "sessionId": session_id_str,
                    "status": s.status,
                    "elapsed": elapsed,
                    "hasResult": s.status.is_terminal() && has_last_message,
                    "hasLastMessage": has_last_message,
                });
                if let Some(ref label) = label_from_summary(&s) {
                    entry["broLabel"] = Value::String(label.clone());
                }
                if s.interrupted {
                    entry["interrupted"] = Value::Bool(true);
                }
                entry
            })
            .collect();
        let mut response =
            json!({"count": entries.len(), "total": total, "offset": offset, "tasks": entries});
        let next_offset = offset.saturating_add(entries.len());
        if next_offset < total {
            response["next_offset"] = json!(next_offset);
        }
        Self::ok_json(&response)
    }

    #[tool(
        name = "bro_providers",
        description = "List provider summaries with current peak_usage advisories; pass provider to list its model slugs and reasoning efforts."
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
        let now = orch::now_ms();
        for p in Provider::ALL {
            if selected.is_some_and(|selected| selected != *p) {
                continue;
            }
            let mut entry = json!({
                "promptCache": p.prompt_cache(),
                "supportsResume": p.supports_resume(),
            });
            super::dispatch::annotate_peak_usage(&mut entry, *p, now);
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
        description = "Manage brofiles and accounts. list/list_accounts return bounded summary pages; get/get_account return exact redaction-safe JSON body pages."
    )]
    pub(crate) async fn bro_brofile(
        &self,
        Parameters(p): Parameters<BrofileParams>,
    ) -> CallToolResult {
        let server = self.clone();
        let project_mutation = matches!(p.action.as_str(), "create" | "delete")
            && p.scope.as_deref() == Some("project")
            && !self.state.project_authority.is_bridge();
        let result =
            match tokio::task::spawn_blocking(move || server.bro_brofile_sync(Parameters(p))).await
            {
                Ok(result) => result,
                Err(error) => return Self::err_text(&format!("brofile task failed: {error}")),
            };
        if project_mutation
            && result.is_error != Some(true)
            && let Err(error) = self.state.persist_checkout_mutations_durable().await
        {
            return Self::err_text(&format!(
                "Error: the brofile edit was queued, but checkout-queue durability failed: {error:#}"
            ));
        }
        result
    }

    fn bro_brofile_sync(&self, Parameters(p): Parameters<BrofileParams>) -> CallToolResult {
        use orchestration::brofile;
        if let Err(error) = validate_brofile_params(&p) {
            return Self::err_text(&error.to_string());
        }
        let exact = matches!(
            p.action.as_str(),
            "get"
                | "get_account"
                | "list_accounts"
                | "get_provider_default"
                | "list_provider_defaults"
        );
        if !exact && (p.cursor.is_some() || p.body_limit.is_some()) {
            return Self::err_text(
                "cursor and body_limit require get, get_account, list_accounts, get_provider_default, or list_provider_defaults",
            );
        }
        if !matches!(p.action.as_str(), "list" | "list_accounts")
            && (p.limit.is_some() || p.offset.is_some())
        {
            return Self::err_text("limit and offset require list or list_accounts");
        }
        if p.project_dir
            .as_deref()
            .is_some_and(|path| path.trim().is_empty())
        {
            return Self::err_text("project_dir must not be blank");
        }
        let store_dir = &self.state.store_dir;
        let scope = match brofile::validate_scope(p.scope.as_deref()) {
            Ok(scope) => scope,
            Err(error) => return Self::err_text(&format!("Error: {error:#}")),
        };
        let project_dir = match (scope, p.project_dir.as_deref()) {
            ("project", Some(dir)) => Some(dir),
            ("project", None) => {
                return Self::err_text("project_dir is required for scope=project");
            }
            ("global", None) => None,
            _ => {
                return Self::err_text(
                    "project_dir applies only to scope=project; omit it for global scope",
                );
            }
        };

        let account_scoped = matches!(
            p.action.as_str(),
            "set_account"
                | "list_accounts"
                | "get_account"
                | "set_provider_default"
                | "get_provider_default"
                | "list_provider_defaults"
                | "clear_provider_default"
        );
        if account_scoped && scope == "project" {
            return Self::err_text(
                "scope=project does not apply to account/provider-default actions; account configuration lives in the daemon-owned store",
            );
        }

        if let Some(project) = project_dir {
            if !Path::new(project).is_absolute() {
                return Self::err_text("project_dir must be an absolute owner-host directory");
            }
            if !self.state.project_authority.is_bridge() {
                return match catalog_project_brofile_action(self, &p, project) {
                    Ok(value) => Self::ok_json(&value),
                    Err(error) => Self::err_text(&format!("Error: {error:#}")),
                };
            }
        }
        let account_config = if account_scoped
            && !matches!(
                p.action.as_str(),
                "set_account" | "set_provider_default" | "clear_provider_default"
            ) {
            match brofile::read_config(store_dir) {
                Ok(config) => Some(config),
                Err(error) => return Self::err_text(&format!("error.config_unavailable: {error}")),
            }
        } else {
            None
        };

        match p.action.as_str() {
            "create" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
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
                if let Err(e) = brofile::save_brofile(&bf, scope, store_dir, project_dir) {
                    return Self::err_text(&format!("brofile save failed: {e}"));
                }
                Self::ok_json(&json!({
                    "created": name,
                    "scope": scope,
                    "summary": brofile::summary_row(&bf),
                    "detail_hint": format!(
                        "bro_brofile(action=\"get\", name=\"{name}\", scope=\"{scope}\") with the same project_dir returns the exact stored brofile as bounded body pages"
                    ),
                }))
            }
            "list" => {
                let list = match brofile::read_brofiles(scope, store_dir, project_dir) {
                    Ok(list) => list,
                    Err(error) => {
                        return Self::err_text(&format!(
                            "error.brofile_store_unavailable: {error}"
                        ));
                    }
                };
                let provider = match p
                    .provider
                    .as_deref()
                    .map(str::parse::<Provider>)
                    .transpose()
                {
                    Ok(provider) => provider,
                    Err(_) => return Self::err_text("Unknown provider"),
                };
                match brofile::list_summary_page(
                    list,
                    provider,
                    p.name.as_deref(),
                    p.offset.unwrap_or(0),
                    p.limit.unwrap_or(20),
                ) {
                    Ok(page) => Self::ok_json(&page),
                    Err(error) => Self::err_text(&format!("brofile inventory: {error}")),
                }
            }
            "get" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match brofile::read_brofile(name, scope, store_dir, project_dir) {
                    Ok(Some(bf)) => {
                        let value = serde_json::to_value(&bf).unwrap_or_default();
                        let selection = brofile_selection(scope, store_dir, project_dir, name);
                        match super::body_page::json_body_page(
                            &selection,
                            &value,
                            p.cursor.as_deref(),
                            p.body_limit,
                        ) {
                            Ok(body) => Self::ok_json(&json!({
                                "name": name,
                                "scope": scope,
                                "summary": brofile::summary_row(&bf),
                                "body": body,
                            })),
                            Err(error) => Self::err_text(&format!("Error: {error:#}")),
                        }
                    }
                    Ok(None) => {
                        Self::err_text(&format!("Brofile not found in {scope} scope: {name}"))
                    }
                    Err(error) => {
                        Self::err_text(&format!("error.brofile_store_unavailable: {error}"))
                    }
                }
            }
            "delete" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match brofile::remove_brofile(name, scope, store_dir, project_dir) {
                    Ok(true) => Self::ok_json(&json!({"deleted":name})),
                    Ok(false) => Self::err_text(&format!("Brofile not found: {name}")),
                    Err(error) => Self::err_text(&format!("Brofile removal failed: {error}")),
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
                    brofile::account_summary_row(name, account)
                }) {
                    Ok(summary) => Self::ok_json(&json!({
                        "account": preview_text(name, 256),
                        "updated": true,
                        "summary": summary,
                        "detail_hint": "bro_brofile(action=list_accounts, body_limit=4096) recovers exact identities; get_account with the original name pages redacted policy",
                    })),
                    Err(error) => Self::err_text(&format!("Configuration was not saved: {error}")),
                }
            }

            "get_account" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match account_config.as_ref().unwrap().accounts.get(name) {
                    Some(account) => {
                        let value = account.response_view();
                        let selection = format!("account:{}:{name}", store_identity(store_dir));
                        match super::body_page::json_body_page(
                            &selection,
                            &value,
                            p.cursor.as_deref(),
                            p.body_limit,
                        ) {
                            Ok(body) => Self::ok_json(&json!({
                                "name": preview_text(name, 256),
                                "body": body,
                            })),
                            Err(error) => Self::err_text(&format!("Error: {error:#}")),
                        }
                    }
                    None => Self::err_text(&format!("Account not found: {name}")),
                }
            }

            "list_accounts" => {
                let config = account_config.as_ref().unwrap();
                if p.cursor.is_some() || p.body_limit.is_some() {
                    if p.limit.is_some() || p.offset.is_some() {
                        return Self::err_text(
                            "exact account inventory uses cursor/body_limit; omit row limit/offset",
                        );
                    }
                    let accounts: std::collections::BTreeMap<_, _> = config
                        .accounts
                        .iter()
                        .map(|(name, account)| (name.clone(), account.response_view()))
                        .collect();
                    let selection = format!("account-inventory:{}", store_identity(store_dir));
                    return match super::body_page::json_body_page(
                        &selection,
                        &json!(accounts),
                        p.cursor.as_deref(),
                        p.body_limit,
                    ) {
                        Ok(body) => Self::ok_json(&json!({"body":body})),
                        Err(error) => Self::err_text(&format!(
                            "account inventory: {error}; restart without cursor"
                        )),
                    };
                }
                match brofile::account_summary_page(
                    &config.accounts,
                    p.offset.unwrap_or(0),
                    p.limit.unwrap_or(20),
                ) {
                    Ok(mut page) => {
                        page["detail_hint"] = json!(
                            "get_account(name) reads one exact redacted account; list_accounts(body_limit=4096) pages the entire redacted inventory including full identities"
                        );
                        Self::ok_json(&page)
                    }
                    Err(error) => Self::err_text(&format!(
                        "account inventory: {error}; use list_accounts(body_limit=4096) for exact bounded recovery"
                    )),
                }
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
                    &json!({"provider":provider.as_str(), "account":preview_text(&account, 256),
                    "updated":true, "detail_hint":"get_provider_default with the same provider pages the exact mapping"}),
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
                let account = account_config
                    .as_ref()
                    .unwrap()
                    .provider_defaults
                    .get(&provider)
                    .map(|entry| &entry.account);
                let mapping = json!({"provider":provider.as_str(), "account":account});
                if p.cursor.is_none()
                    && p.body_limit.is_none()
                    && serde_json::to_vec(&mapping).is_ok_and(|b| b.len() < 1024)
                {
                    return Self::ok_json(&mapping);
                }
                match super::body_page::json_body_page(
                    &format!("provider-default:{}:{provider}", store_identity(store_dir)),
                    &mapping,
                    p.cursor.as_deref(),
                    p.body_limit,
                ) {
                    Ok(body) => Self::ok_json(&json!({"provider":provider.as_str(),"body":body})),
                    Err(error) => Self::err_text(&format!("provider default page: {error}")),
                }
            }
            "list_provider_defaults" => {
                let config = account_config.unwrap();
                let defaults: std::collections::HashMap<String, String> = config
                    .provider_defaults
                    .into_iter()
                    .map(|(provider, entry)| (provider.to_string(), entry.account))
                    .collect();
                let value = json!(defaults);
                if p.cursor.is_none()
                    && p.body_limit.is_none()
                    && serde_json::to_vec(&value).is_ok_and(|b| b.len() < 2048)
                {
                    return Self::ok_json(&value);
                }
                match super::body_page::json_body_page(
                    &format!("provider-defaults:{}", store_identity(store_dir)),
                    &value,
                    p.cursor.as_deref(),
                    p.body_limit,
                ) {
                    Ok(body) => Self::ok_json(&json!({"body":body})),
                    Err(error) => Self::err_text(&format!("provider defaults page: {error}")),
                }
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

    /// Stamp a named dispatch's brofile name on its task so tail, wait and
    /// dashboard output name it.
    pub(crate) fn record_task_to_bro(&self, bro_name: &str, task: &Arc<orch::Task>) {
        task.inner.lock().bro_label = Some(bro_name.to_string());
    }
}

/// Canonical store identity for content-bound cursors. The digest inside a
/// cursor never discloses the path; canonicalization keeps two distinct
/// aliases of one store from splitting identity, falling back to the raw
/// path when the store is not resolvable.
#[allow(
    clippy::disallowed_methods,
    reason = "bro_brofile runs its store operations on spawn_blocking; tests use isolated fixture directories"
)]
fn validate_brofile_params(p: &BrofileParams) -> anyhow::Result<()> {
    let action = p.action.as_str();
    anyhow::ensure!(
        matches!(
            action,
            "create"
                | "list"
                | "get"
                | "delete"
                | "get_account"
                | "set_account"
                | "list_accounts"
                | "get_provider_default"
                | "set_provider_default"
                | "list_provider_defaults"
                | "clear_provider_default"
        ),
        "Unknown brofile action"
    );
    if matches!(action, "create" | "get" | "delete") {
        let name = p
            .name
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("name is required"))?;
        validate_object_name(name)?;
    }
    anyhow::ensure!(
        action == "create"
            || (p.lens.is_none()
                && p.model.is_none()
                && p.effort.is_none()
                && p.tool_defaults.is_none()
                && p.allow_tools.is_none()
                && p.disallow_tools.is_none()
                && p.surface.is_none()
                && p.context.is_none()
                && p.code_mode.is_none()
                && p.service_tier.is_none()),
        "persona configuration fields require action=create"
    );
    anyhow::ensure!(
        action == "set_account" || p.env.is_none(),
        "env requires action=set_account"
    );
    anyhow::ensure!(
        matches!(action, "create" | "set_provider_default") || p.account.is_none(),
        "account requires action=create or set_provider_default"
    );
    anyhow::ensure!(
        matches!(
            action,
            "create"
                | "list"
                | "get_provider_default"
                | "set_provider_default"
                | "clear_provider_default"
        ) || p.provider.is_none(),
        "provider does not apply to this action"
    );
    anyhow::ensure!(
        matches!(
            action,
            "create" | "list" | "get" | "delete" | "get_account" | "set_account"
        ) || p.name.is_none(),
        "name does not apply to this action"
    );
    Ok(())
}

/// Body-page selection for `bro_brofile get`. Binds scope, the selected
/// store's identity, and the brofile name so identical bodies stored in
/// distinct project (or global) stores cannot accept one another's cursors.
fn brofile_selection(
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
    name: &str,
) -> String {
    let store = match (scope, project_dir) {
        ("project", Some(dir)) => store_identity(Path::new(dir)),
        _ => store_identity(store_dir),
    };
    format!("brofile:{scope}:{store}:{name}")
}

const PROJECT_BROFILE_PUBLICATION: &str = "Project brofiles are read from the project's accepted publication; a brofile queued through create or delete takes effect for reads and dispatch only after the checkout owner commits and publishes it";

/// Catalog-mode project brofile actions. `project_dir` only selects a catalog
/// project; reads come from its accepted configuration and edits are guarded
/// mutations for its checkout owner. Exact project scope never includes
/// global brofiles.
fn catalog_project_brofile_action(
    server: &BlackboxServer,
    p: &BrofileParams,
    selector: &str,
) -> anyhow::Result<Value> {
    use crate::tools::project_config::ProjectConfigEdit;
    use orchestration::brofile;
    let provider = match p.provider.as_deref() {
        None => None,
        Some(value) => Some(value.parse::<Provider>().map_err(|_| {
            anyhow::anyhow!(if p.action == "create" {
                "valid provider is required"
            } else {
                "Unknown provider"
            })
        })?),
    };
    let name = p.name.as_deref();
    let target = match name {
        Some(name) if p.action != "list" => Some(project_brofile_target(name)?),
        _ => None,
    };
    let accepted = server.state.select_project_config_scope(selector)?;
    let snapshot = &accepted.snapshot;
    let project_id = accepted.project_id.as_str();
    let source = json!(snapshot.provenance());
    match p.action.as_str() {
        "list" => {
            let list = snapshot
                .brofiles()
                .map(|(_, brofile)| brofile.clone())
                .collect();
            let mut page = brofile::list_summary_page(
                list,
                provider,
                name,
                p.offset.unwrap_or(0),
                p.limit.unwrap_or(20),
            )
            .map_err(|error| anyhow::anyhow!("brofile inventory: {error}"))?;
            page["scope"] = json!("project");
            page["projectId"] = json!(project_id);
            page["source"] = source;
            page["publication"] = json!(PROJECT_BROFILE_PUBLICATION);
            Ok(page)
        }
        "get" => {
            let name = name.expect("validated brofile name");
            let found = snapshot.brofile(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Brofile not found in project scope: {name}. Project {project_id}'s accepted brofiles do not include it; use list in the same scope. {PROJECT_BROFILE_PUBLICATION}"
                )
            })?;
            let selection = json!(["brofile", "project", project_id, name]).to_string();
            let body = super::body_page::json_body_page(
                &selection,
                &serde_json::to_value(found)?,
                p.cursor.as_deref(),
                p.body_limit,
            )?;
            Ok(json!({
                "name": name,
                "scope": "project",
                "projectId": project_id,
                "source": source,
                "summary": brofile::summary_row(found),
                "body": body,
            }))
        }
        "create" => {
            let name = name.expect("validated brofile name");
            let provider = provider.ok_or_else(|| anyhow::anyhow!("valid provider is required"))?;
            let bf = brofile::Brofile {
                name: name.to_string(),
                provider,
                account: p.account.clone(),
                lens: p.lens.clone(),
                model: p.model.clone(),
                effort: p.effort.clone(),
                tool_defaults: p.tool_defaults.clone(),
                filters: extra_filters_from_params(
                    p.allow_tools.as_deref(),
                    p.disallow_tools.as_deref(),
                ),
                surface: p.surface.clone(),
                coerce_workspace: p.coerce_workspace,
                runtime: None,
                context: p.context.clone(),
                code_mode: p.code_mode,
                service_tier: p.service_tier.clone(),
            };
            // The same bytes a bridge-mode create writes to the checkout.
            let content = serde_json::to_string_pretty(&bf)?;
            let mut replaces = false;
            let receipt = server.state.prepare_project_config_mutation(
                project_id,
                target.as_ref().expect("create names its target"),
                "bro_brofile(action=create, scope=project)",
                |base| {
                    replaces = base.is_some();
                    Ok(Some(ProjectConfigEdit::Write(content)))
                },
            )?;
            let mut value = json!({
                "created": name,
                "replaces": replaces,
                "summary": brofile::summary_row(&bf),
            });
            project_brofile_receipt(
                &mut value,
                project_id,
                receipt,
                "The project's accepted brofile, with its queued edits applied, already has these exact bytes; nothing was queued",
            );
            Ok(value)
        }
        "delete" => {
            let name = name.expect("validated brofile name");
            let mut present = false;
            let receipt = server.state.prepare_project_config_mutation(
                project_id,
                target.as_ref().expect("delete names its target"),
                "bro_brofile(action=delete, scope=project)",
                |base| {
                    present = base.is_some();
                    Ok(Some(ProjectConfigEdit::Delete))
                },
            )?;
            anyhow::ensure!(
                present,
                "Brofile not found: {name}. Neither project {project_id}'s accepted brofiles nor its queued edits hold it; nothing was queued"
            );
            let mut value = json!({"deleted": name});
            project_brofile_receipt(&mut value, project_id, receipt, "");
            Ok(value)
        }
        _ => unreachable!("only project brofile actions reach the catalog lane"),
    }
}

fn project_brofile_target(name: &str) -> anyhow::Result<bbox_code_source::ProjectConfigTargetV1> {
    bbox_code_source::validate_project_config_name(name).map_err(|error| {
        anyhow::anyhow!("project brofile name cannot be a project configuration file: {error}")
    })?;
    Ok(bbox_code_source::ProjectConfigTargetV1::Brofile(
        name.to_string(),
    ))
}

fn project_brofile_receipt(
    receipt: &mut Value,
    project_id: &str,
    mutation: Option<crate::tools::project_config::ProjectConfigMutationReceipt>,
    unchanged: &str,
) {
    receipt["scope"] = json!("project");
    receipt["projectId"] = json!(project_id);
    match mutation {
        Some(mutation) => {
            receipt["state"] = json!(mutation.state);
            receipt["mutation"] = json!(mutation);
        }
        None => {
            receipt["state"] = json!("unchanged");
            receipt["detail"] = json!(unchanged);
        }
    }
}

#[cfg(test)]
#[path = "roster_brofile_project_tests.rs"]
mod brofile_project_tests;

fn validate_object_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0']),
        "name must be an exact stored brofile name, not a path"
    );
    Ok(())
}

/// Pick the `broLabel` value for a `bro_dashboard` row:
/// `RosterSummaryV1.label`, the task's named-dispatch bro identity. The
/// summary's `name` field is the daemon display name and can match, but
/// `label` is the field-by-field source.
fn label_from_summary(s: &bro_protocol::RosterSummaryV1) -> Option<String> {
    s.label.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::server::state::SharedState;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
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
            let args = match action {
                "set_account" => json!({"action":action,"name":"synthetic"}),
                "set_provider_default" => {
                    json!({"action":action,"provider":"brodex","account":"synthetic"})
                }
                _ => json!({"action":action,"provider":"brodex"}),
            };
            let params: BrofileParams = serde_json::from_value(args).unwrap();
            let result = server.bro_brofile_sync(Parameters(params));
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
        let result = server.bro_brofile_sync(Parameters(params));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            std::fs::read_to_string(path.join("retained")).unwrap(),
            "existing-data"
        );
    }

    #[test]
    fn configuration_reads_report_corruption_without_empty_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let path = server.state.store_dir.join("config.json");
        std::fs::write(&path, b"{synthetic corrupt account").unwrap();
        for args in [
            json!({"action":"list_accounts"}),
            json!({"action":"get_account","name":"synthetic"}),
            json!({"action":"get_provider_default","provider":"brodex"}),
            json!({"action":"list_provider_defaults"}),
        ] {
            let reply = server.bro_brofile_sync(Parameters(serde_json::from_value(args).unwrap()));
            assert_eq!(reply.is_error, Some(true));
            assert!(extract_text(&reply).contains("config_unavailable"));
        }
        let dir = server.state.store_dir.join("brofiles");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("synthetic.json"), b"{bad brofile").unwrap();
        for args in [
            json!({"action":"list"}),
            json!({"action":"get","name":"synthetic"}),
        ] {
            let reply = server.bro_brofile_sync(Parameters(serde_json::from_value(args).unwrap()));
            assert_eq!(reply.is_error, Some(true));
            assert!(extract_text(&reply).contains("brofile_store_unavailable"));
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"{synthetic corrupt account");
    }

    #[test]
    fn catalog_brofile_project_calls_select_a_catalog_project_without_filesystem_access() {
        let fixture = crate::server::state::catalog_fixture::CatalogFixture::new();
        let server = fixture.server();
        // A daemon-host directory that names no catalog project is no project
        // scope: nothing is read from it, and nothing is queued for it.
        let local = fixture.root().join("synthetic-owner-checkout");
        std::fs::create_dir_all(local.join(".bro/brofiles")).unwrap();
        let path = local.join(".bro/brofiles/synthetic.json");
        std::fs::write(&path, br#"{"name":"synthetic","provider":"brodex"}"#).unwrap();
        for action in ["list", "get", "create", "delete"] {
            let mut args = json!({"action":action,"scope":"project","project_dir":local});
            if action != "list" {
                args["name"] = json!("synthetic");
            }
            if action == "create" {
                args["provider"] = json!("brodex");
            }
            let reply = server.bro_brofile_sync(Parameters(serde_json::from_value(args).unwrap()));
            assert_eq!(reply.is_error, Some(true));
            let text = extract_text(&reply);
            assert!(
                text.contains("error.project_config_project_unknown"),
                "{text}"
            );
            assert!(!text.contains("brofile_locality_required"), "{text}");
        }
        assert!(
            std::fs::read(&path)
                .unwrap()
                .starts_with(br#"{"name":"synthetic""#)
        );
        assert_eq!(server.state.checkout_mutations.read().pending_count(), 0);
    }

    #[test]
    fn large_provider_default_mutation_has_bounded_receipt_and_exact_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let account = "synthetic-界\"".repeat(4000);
        let reply = server.bro_brofile_sync(Parameters(
            serde_json::from_value(json!({
                "action":"set_provider_default","provider":"brodex","account":account
            }))
            .unwrap(),
        ));
        assert_ne!(reply.is_error, Some(true));
        assert!(serde_json::to_vec(&reply).unwrap().len() < 4096);
        let mut cursor = None;
        let mut recovered = String::new();
        loop {
            let reply = server.bro_brofile_sync(Parameters(serde_json::from_value(json!({
                "action":"get_provider_default","provider":"brodex","body_limit":4096,"cursor":cursor
            })).unwrap()));
            assert_ne!(reply.is_error, Some(true));
            assert!(serde_json::to_vec(&reply).unwrap().len() < 16 * 1024);
            let page: Value = serde_json::from_str(&extract_text(&reply)).unwrap();
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let mapping: Value = serde_json::from_str(&recovered).unwrap();
        assert_eq!(mapping["account"], account);
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
        let result = server.bro_brofile_sync(Parameters(params));
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
        for request in [
            json!({"action":"set_account", "name":"synthetic", "env":{"TOKEN":"synthetic-secret"}}),
            json!({"action":"set_provider_default", "provider":"brodex", "account":"synthetic"}),
            json!({"action":"clear_provider_default", "provider":"brodex"}),
        ] {
            let action = request["action"].as_str().unwrap().to_string();
            let params: BrofileParams = serde_json::from_value(request).unwrap();
            let result = server.bro_brofile_sync(Parameters(params));
            assert_eq!(result.is_error, Some(true), "{action} claimed success");
            let response = extract_text(&result);
            assert!(response.contains("not saved"), "{action}: {response}");
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
        let result = server.bro_brofile_sync(Parameters(params));
        assert_ne!(result.is_error, Some(true));
        let set_reply = extract_text(&result);
        let params: BrofileParams =
            serde_json::from_value(json!({"action": "list_accounts"})).unwrap();
        let list_reply = extract_text(&server.bro_brofile_sync(Parameters(params)));
        for reply in [&set_reply, &list_reply] {
            assert!(!reply.contains("synthetic-secret"));
            assert!(!reply.contains("opaque-secret"));
            assert!(reply.contains("TOKEN"));
            assert!(reply.contains("CUSTOM"));
        }
        let list: Value = serde_json::from_str(&list_reply).unwrap();
        let rows = list["accounts"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "synthetic");
        assert_eq!(rows[0]["env_keys"], json!(["CUSTOM", "TOKEN"]));
        assert_eq!(list["total"], 1);
        let account =
            orchestration::brofile::load_account("synthetic", &server.state.store_dir).unwrap();
        assert_eq!(account.env.unwrap()["TOKEN"], "synthetic-secret");
    }

    #[test]
    fn brofile_scope_validation_rejects_unknown_and_ambiguous_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cases = [
            (Some("typo"), None, "Unknown scope"),
            (Some("project"), None, "project_dir is required"),
            (Some("global"), Some("/nonexistent/project"), "applies only"),
        ];
        for (scope, project_dir, expected) in cases {
            let params: BrofileParams = serde_json::from_value(json!({
                "action": "list", "scope": scope, "project_dir": project_dir,
            }))
            .unwrap();
            let result = server.bro_brofile_sync(Parameters(params));
            assert_eq!(result.is_error, Some(true), "{scope:?} {project_dir:?}");
            let text = extract_text(&result);
            assert!(text.contains(expected), "{scope:?}: {text}");
        }
    }

    #[test]
    fn brofile_get_reads_only_the_requested_store() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let shared: orchestration::brofile::Brofile = serde_json::from_value(json!({
            "name": "shared", "provider": "glm", "lens": "global lens",
        }))
        .unwrap();
        let override_bf: orchestration::brofile::Brofile = serde_json::from_value(json!({
            "name": "shared", "provider": "glm", "lens": "project lens",
        }))
        .unwrap();
        orchestration::brofile::save_brofile(&shared, "global", &server.state.store_dir, None)
            .unwrap();
        orchestration::brofile::save_brofile(
            &override_bf,
            "project",
            &server.state.store_dir,
            Some(project.to_str().unwrap()),
        )
        .unwrap();

        let project_get: BrofileParams = serde_json::from_value(json!({
            "action": "get", "name": "shared", "scope": "project",
            "project_dir": project.to_str().unwrap(),
        }))
        .unwrap();
        let reply = extract_text(&server.bro_brofile_sync(Parameters(project_get)));
        assert!(reply.contains("project lens"), "{reply}");
        assert!(!reply.contains("global lens"), "{reply}");

        let global_only: BrofileParams = serde_json::from_value(json!({
            "action": "get", "name": "override-missing", "scope": "global",
        }))
        .unwrap();
        let reply = extract_text(&server.bro_brofile_sync(Parameters(global_only)));
        assert!(reply.contains("not found in global scope"), "{reply}");
    }

    #[test]
    fn brofile_get_body_pages_bind_store_identity_and_recover_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_a = root.join("project-a");
        let project_b = root.join("project-b");
        for dir in [&project_a, &project_b] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let mut tool_defaults = serde_json::Map::new();
        for i in 0..200 {
            tool_defaults.insert(format!("tool_{i}"), json!(format!("granted-{i}")));
        }
        let bf: orchestration::brofile::Brofile = serde_json::from_value(json!({
            "name": "twin", "provider": "glm", "lens": "l".repeat(20000),
            "tool_defaults": tool_defaults,
        }))
        .unwrap();
        for dir in [&project_a, &project_b] {
            orchestration::brofile::save_brofile(
                &bf,
                "project",
                &server.state.store_dir,
                Some(dir.to_str().unwrap()),
            )
            .unwrap();
        }

        let page: BrofileParams = serde_json::from_value(json!({
            "action": "get", "name": "twin", "scope": "project",
            "project_dir": project_a.to_str().unwrap(),
        }))
        .unwrap();
        let first = extract_text(&server.bro_brofile_sync(Parameters(page)));
        let first: Value = serde_json::from_str(&first).unwrap();
        assert!(first["body"]["text"].as_str().unwrap().len() <= 4096);
        let cursor = first["body"]["next_cursor"].as_str().unwrap().to_owned();

        // Identical bodies in distinct project stores must not accept one
        // another's cursors: the selection digest binds the store identity.
        let cross_store: BrofileParams = serde_json::from_value(json!({
            "action": "get", "name": "twin", "scope": "project",
            "project_dir": project_b.to_str().unwrap(), "cursor": cursor,
        }))
        .unwrap();
        let result = server.bro_brofile_sync(Parameters(cross_store));
        assert_eq!(result.is_error, Some(true));

        let mut reconstructed = first["body"]["text"].as_str().unwrap().to_owned();
        let mut cursor = Some(cursor);
        while let Some(current) = cursor {
            let params: BrofileParams = serde_json::from_value(json!({
                "action": "get", "name": "twin", "scope": "project",
                "project_dir": project_a.to_str().unwrap(), "cursor": current,
            }))
            .unwrap();
            let page: Value =
                serde_json::from_str(&extract_text(&server.bro_brofile_sync(Parameters(params))))
                    .unwrap();
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
        }
        let recovered: orchestration::brofile::Brofile =
            serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(recovered.name, "twin");
        assert_eq!(recovered.lens.as_deref(), Some("l".repeat(20000).as_str()));
        assert_eq!(recovered.tool_defaults.as_ref().unwrap().len(), 200);
    }

    #[test]
    fn account_inventory_bounds_long_identities_and_recovers_them_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let name = "\u{0001}🦀".repeat(600);
        let env_key = "LONG_KEY_".repeat(100);
        let mut config = orchestration::brofile::BroConfig::default();
        config.accounts.insert(
            name.clone(),
            orchestration::brofile::Account {
                env: Some(std::collections::HashMap::from([(
                    env_key.clone(),
                    "credential-sentinel".into(),
                )])),
                ..Default::default()
            },
        );
        orchestration::brofile::save_config(&config, &server.state.store_dir).unwrap();
        let response = server.bro_brofile_sync(Parameters(
            serde_json::from_value(json!({"action":"list_accounts"})).unwrap(),
        ));
        assert_ne!(response.is_error, Some(true));
        assert!(serde_json::to_vec(&response).unwrap().len() < 4096);
        let summary: Value = serde_json::from_str(&extract_text(&response)).unwrap();
        assert!(summary["accounts"][0].get("name").is_none());
        assert!(summary["accounts"][0]["name_preview"].is_string());
        let mut joined = String::new();
        let mut cursor = None;
        loop {
            let response = server.bro_brofile_sync(Parameters(
                serde_json::from_value(
                    json!({"action":"list_accounts", "body_limit":512,"cursor":cursor}),
                )
                .unwrap(),
            ));
            assert_ne!(response.is_error, Some(true));
            let text = extract_text(&response);
            assert!(!text.contains("credential-sentinel"));
            let page: Value = serde_json::from_str(&text).unwrap();
            joined.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let exact: Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(exact.as_object().unwrap().len(), 1);
        assert_eq!(exact[&name], config.accounts[&name].response_view());
    }

    #[test]
    fn account_pages_bound_single_oversized_row_and_recover_redacted_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let mut env = std::collections::HashMap::new();
        for i in 0..5000 {
            env.insert(format!("KEY_{i:04}"), format!("value-synthetic-{i}"));
        }
        let mut config = orchestration::brofile::BroConfig::default();
        config.accounts.insert(
            "oversized".into(),
            orchestration::brofile::Account {
                env: Some(env),
                allowed_models: vec![format!("model-{}", "m".repeat(64))],
                ..Default::default()
            },
        );
        orchestration::brofile::save_config(&config, &server.state.store_dir).unwrap();

        let params: BrofileParams =
            serde_json::from_value(json!({"action": "list_accounts"})).unwrap();
        let reply = extract_text(&server.bro_brofile_sync(Parameters(params)));
        assert!(!reply.contains("value-synthetic"), "{reply}");
        let page: Value = serde_json::from_str(&reply).unwrap();
        let row = &page["accounts"][0];
        assert_eq!(row["name"], "oversized");
        assert_eq!(row["env_key_count"], 5000);
        assert_eq!(row["env_keys"].as_array().unwrap().len(), 20);
        assert_eq!(row["env_keys_omitted"], 4980);
        assert!(
            serde_json::to_vec(&page).unwrap().len() < 2048,
            "oversized row leaked into summary page"
        );

        let mut reconstructed = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut request = json!({"action": "get_account", "name": "oversized"});
            if let Some(current) = cursor.as_deref() {
                request["cursor"] = json!(current);
            }
            let params: BrofileParams = serde_json::from_value(request).unwrap();
            let page: Value =
                serde_json::from_str(&extract_text(&server.bro_brofile_sync(Parameters(params))))
                    .unwrap();
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert!(!reconstructed.contains("value-synthetic"));
        let view: Value = serde_json::from_str(&reconstructed).unwrap();
        let keys = view["env_keys"].as_array().unwrap();
        assert_eq!(keys.len(), 5000);
        assert_eq!(keys[0], "KEY_0000");
        assert_eq!(keys[4999], "KEY_4999");
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
        assert!(summary["glm"]["peak_usage"].is_boolean());
        assert!(summary["deepseek"]["peak_usage"].is_boolean());
        assert!(summary["brodex"].get("peak_usage").is_none());
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
        use bro_protocol::RosterSummaryV1;
        use bro_protocol::TaskStatus as WireTaskStatus;

        fn live_summary(id: &str, provider: Provider, started_at: u64) -> RosterSummaryV1 {
            RosterSummaryV1 {
                task_id: TaskId::new(id),
                status: WireTaskStatus::Running,
                provider,
                cost: Some(0.10),
                turns: Some(4),
                cwd: Some("/work/alpha".to_string()),
                label: Some("executor".to_string()),
                name: Some("Inspect the failing roster columns".to_string()),
                session_id: Some(SessionId::new(format!("sess-{id}"))),
                has_last_message: None,
                last_message_snippet: Some("hello".to_string()),
                model: Some("glm-pro".to_string()),
                last_event_at: Some(started_at),
                origin: Origin::Cockpit,
                managed_worktree: Some("/wt/alpha".to_string()),
                workflow_owned: false,
                started_at: Some(started_at),
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
                label: Some("reviewer".to_string()),
                name: Some(format!("Prompt teaser {id}")),
                session_id: Some(SessionId::new(format!("sess-{id}"))),
                has_last_message: None,
                last_message_snippet: None,
                model: None,
                last_event_at: Some(completed_at),
                origin: Origin::AgentDispatch,
                managed_worktree: None,
                workflow_owned: false,
                started_at: Some(started_at),
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
            assert_eq!(live["broLabel"], "executor");
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
            assert_eq!(term["broLabel"], "reviewer");
            assert_eq!(term["elapsed"], "1s");
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
        fn dashboard_pagination_pages_tasks_in_start_order() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            for idx in 0..4 {
                let id = format!("task-{idx}");
                let summary = live_summary(&id, Provider::Glm, 1000 + idx);
                server.state.roster_view.upsert(id, summary);
            }
            let read_page = |offset| {
                let response = server.bro_dashboard(Parameters(DashboardParams {
                    provider: None,
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
            assert!(serde_json::to_vec(&first).unwrap().len() < 4096);
        }

        #[test]
        fn dashboard_invalid_filters_do_not_broaden_the_selection() {
            let tmp = tempfile::tempdir().unwrap();
            let server = test_server(&tmp);
            for (provider, status) in [(Some("unknown"), None), (None, Some("done"))] {
                let response = server.bro_dashboard(Parameters(DashboardParams {
                    provider: provider.map(str::to_string),
                    status: status.map(str::to_string),
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
        fn dashboard_omits_context_telemetry_even_when_present() {
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
            assert!(row.get("context").is_none());
            assert!(body.get("context_hint").is_none());
            assert_eq!(row["status"], "running");

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
                    inner.bro_label = Some("executor".into());
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
                    inner.bro_label = Some("reviewer".into());
                    inner.last_assistant_message =
                        Some("retained output without a recoverable preview".into());
                    assert!(inner.latest_assistant_preview.text().is_none());
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
            assert_eq!(live["broLabel"], "executor");
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
            assert_eq!(done["broLabel"], "reviewer");
            assert_eq!(done["elapsed"], "1s");
            assert_eq!(done["hasResult"], true);
            assert_eq!(done["hasLastMessage"], true);
        }
    }
}
