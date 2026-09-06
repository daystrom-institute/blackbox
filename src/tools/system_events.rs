use crate::server::BlackboxServer;
use crate::system_events;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::system_events_tools()
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SystemEventEmitParams {
    pub kind: String,
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    /// Optional `EventPrincipal` JSON: `{"kind": "...", "bro": "...", "provider": "...", "model": "...", "effort": "..."}`.
    /// Only `kind` is required. Parsed to the typed `EventPrincipal` struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<serde_json::Value>,
    /// Optional `EventSubject` JSON: `{"kind": "...", "id": "..."}`. Both fields required when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<serde_json::Value>,
    /// Optional correlation map. Free-form keys; values may be any JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SystemEventListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Continue from next_before returned by the previous page. New appends do not shift continuation.
    #[serde(default)]
    pub before: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SystemEventOpenParams {
    pub event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SystemEventCompactParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReactionInstallParams {
    #[schemars(with = "serde_json::Map<String, serde_json::Value>")]
    pub spec: serde_json::Value,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReactionListParams {
    /// Exact installed reaction/source name filter.
    #[serde(default)]
    pub name: Option<String>,
    /// Default 20, maximum 100 rows.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue from next_offset.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Policy diagnostics only; action arguments and credentials stay omitted.
    #[serde(default)]
    pub detail: bool,
    /// reactions (default) or warnings, each independently paginated.
    #[serde(default)]
    pub view: ReactionListView,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReactionListView {
    #[default]
    Reactions,
    Warnings,
}

fn reaction_catalog_page(
    result: system_events::hub::ReactionListResult,
    p: &ReactionListParams,
) -> anyhow::Result<serde_json::Value> {
    let warning_count = result.warnings.len();
    let (field, mut rows): (&str, Vec<serde_json::Value>) = match p.view {
        ReactionListView::Reactions => (
            "reactions",
            result
                .reactions
                .iter()
                .map(|spec| spec.response_view(p.detail))
                .collect(),
        ),
        ReactionListView::Warnings => (
            "warnings",
            result
                .warnings
                .iter()
                .map(|warning| {
                    // A source basename identifies the installed configuration without
                    // asking callers to read a file on the daemon host.
                    let name = std::path::Path::new(&warning.source)
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("unknown");
                    let reason = if warning.reason.starts_with("parse error") {
                        "invalid_json"
                    } else if warning.reason.starts_with("validation failed") {
                        "invalid_reaction"
                    } else {
                        "load_error"
                    };
                    serde_json::json!({"name": name, "reason": reason})
                })
                .collect(),
        ),
    };
    rows.retain(|row| {
        p.name
            .as_deref()
            .is_none_or(|name| row["name"].as_str() == Some(name))
    });
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let total = rows.len();
    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let rows: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
    let mut page = serde_json::json!({"total": total, "offset": offset, "limit": limit, "count": rows.len(), "warning_count": warning_count});
    page[field] = serde_json::json!(rows);
    bbox_corpus_core::response_page::bound_page(page, field)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReactionReplayParams {
    #[serde(default = "default_dry_run")]
    pub mode: String,
    pub event_id: String,
    pub reaction: String,
}

fn default_dry_run() -> String {
    "dry_run".to_string()
}

fn block_on_tool_future<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    tokio::runtime::Handle::current().block_on(future)
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReactionExecuteParams {
    pub event_id: String,
    pub reaction: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReactionDeliveriesParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReactionRetryParams {
    pub outbox_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct IdentityListParams {}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct IdentityGetParams {
    pub scope: String,
    pub instance: String,
    pub subject: String,
    pub provider: String,
    pub model: String,
}

/// Convert `SystemEventEmitParams` into a typed `SystemEventDraft`.
/// Parses optional `principal`/`subject` JSON Values into the typed structs,
/// defaults correlation to empty, and preserves the rest verbatim.
pub(crate) fn draft_from_emit_params(
    p: SystemEventEmitParams,
) -> anyhow::Result<system_events::SystemEventDraft> {
    let principal = match p.principal {
        Some(v) => Some(
            serde_json::from_value::<system_events::types::EventPrincipal>(v)
                .map_err(|e| anyhow::anyhow!("invalid principal: {e}"))?,
        ),
        None => None,
    };
    let subject = match p.subject {
        Some(v) => Some(
            serde_json::from_value::<system_events::types::EventSubject>(v)
                .map_err(|e| anyhow::anyhow!("invalid subject: {e}"))?,
        ),
        None => None,
    };
    Ok(system_events::SystemEventDraft {
        kind: system_events::types::SystemEventKind::from_wire(&p.kind),
        producer: p.producer,
        project: p.project,
        principal,
        subject,
        correlation: p.correlation.unwrap_or_default(),
        causation_id: p.causation_id,
        payload: p.payload,
    })
}

#[tool_router(router = system_events_tools)]
impl BlackboxServer {
    #[tool(
        name = "system_event_emit",
        description = "Emit a synthetic system event into the journal and broadcast. Ops-only; surface-enforced."
    )]
    pub(crate) async fn tool_system_event_emit(
        &self,
        Parameters(p): Parameters<SystemEventEmitParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("system_event_emit", move || {
            block_on_tool_future(async {
                let draft = draft_from_emit_params(p)?;
                let outcome = server.state.system_events.emit(draft).await?;
                Ok(serde_json::to_string_pretty(&outcome)?)
            })
        })
        .await
    }

    #[tool(
        name = "system_event_list",
        description = "List journal event summaries newest first (default 20, maximum 100). Continue with next_before as before; keep filters unchanged. A missing/compacted cursor errors. Filters match recorded kind/producer/project tags exactly. Payload, correlation, principals, and host project paths are omitted; system_event_open(event_id) expands one event."
    )]
    pub(crate) fn tool_system_event_list(
        &self,
        Parameters(p): Parameters<SystemEventListParams>,
    ) -> CallToolResult {
        Self::run("system_event_list", || {
            let events = self.state.system_events.list_event_page(
                p.limit,
                p.before.as_deref(),
                p.kind.as_deref(),
                p.producer.as_deref(),
                p.project.as_deref(),
            )?;
            Ok(serde_json::to_string_pretty(&events)?)
        })
    }

    #[tool(
        name = "system_event_open",
        description = "Open a single system event with causation chain and derived event links. Readonly."
    )]
    pub(crate) fn tool_system_event_open(
        &self,
        Parameters(p): Parameters<SystemEventOpenParams>,
    ) -> CallToolResult {
        Self::run("system_event_open", || {
            let event = self.state.system_events.open_event(&p.event_id)?;
            let Some(event) = event else {
                anyhow::bail!("event '{}' not found", p.event_id);
            };
            let causation = self.state.system_events.causation_chain_for(&p.event_id)?;
            let derived = self.state.system_events.derived_events(&p.event_id)?;
            let result = serde_json::json!({
                "event": event,
                "causation_chain": causation,
                "derived_events": derived,
            });
            Ok(serde_json::to_string_pretty(&result)?)
        })
    }

    #[tool(
        name = "system_event_compact",
        description = "Apply system-event journal and outbox retention compaction. Ops-only; surface-enforced."
    )]
    pub(crate) async fn tool_system_event_compact(
        &self,
        Parameters(p): Parameters<SystemEventCompactParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("system_event_compact", move || {
            let now = p.now.unwrap_or_else(crate::util::now_iso);
            let report = server.state.system_events.compact_with_now(&now)?;
            Ok(serde_json::to_string_pretty(&report)?)
        })
        .await
    }

    #[tool(
        name = "reaction_install",
        description = "Install a reaction spec. Ops-only. Validates and persists to disk."
    )]
    pub(crate) async fn tool_reaction_install(
        &self,
        Parameters(p): Parameters<ReactionInstallParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("reaction_install", move || {
            block_on_tool_future(async {
                let spec: system_events::types::ReactionSpec = serde_json::from_value(p.spec)?;
                server
                    .state
                    .system_events
                    .install_reaction(spec, p.replace)
                    .await?;
                Ok("installed".to_string())
            })
        })
        .await
    }

    #[tool(
        name = "reaction_list",
        description = "List reactions in name order as summary pages (default 20, maximum 100). Filter exact name; continue with next_offset. detail=true expands event kinds and retry/failure policies, never action arguments or credentials. warning_count reports invalid stored specs; view=warnings pages safe warning names and categories without host paths."
    )]
    pub(crate) async fn tool_reaction_list(
        &self,
        Parameters(p): Parameters<ReactionListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("reaction_list", move || {
            block_on_tool_future(async {
                let result = server
                    .state
                    .system_events
                    .list_reactions_with_warnings()
                    .await;
                Ok(serde_json::to_string_pretty(&reaction_catalog_page(
                    result, &p,
                )?)?)
            })
        })
        .await
    }

    #[tool(
        name = "reaction_replay",
        description = "Dry-run replay a reaction against an event. Returns rendered outputs without executing side effects."
    )]
    pub(crate) async fn tool_reaction_replay(
        &self,
        Parameters(p): Parameters<ReactionReplayParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("reaction_replay", move || {
            block_on_tool_future(async {
                if p.mode != "dry_run" {
                    anyhow::bail!("only mode='dry_run' is supported in this phase");
                }
                let event = server.state.system_events.open_event(&p.event_id)?;
                let Some(event) = event else {
                    anyhow::bail!("event '{}' not found", p.event_id);
                };
                let reaction = server.state.system_events.get_reaction(&p.reaction).await;
                let Some(reaction) = reaction else {
                    anyhow::bail!("reaction '{}' not found", p.reaction);
                };
                let outbox_records = server.state.system_events.outbox_store().load_all()?;
                let packets = server.state.packets.read();
                let result =
                    system_events::dry_run_replay(&reaction, &event, &packets, &outbox_records)?;
                Ok(serde_json::to_string_pretty(&result)?)
            })
        })
        .await
    }

    #[tool(
        name = "reaction_execute",
        description = "Execute a reaction once against an event through the audited outbox path. Ops-only. Set force=true to bypass succeeded-idempotency suppression."
    )]
    pub(crate) async fn tool_reaction_execute(
        &self,
        Parameters(p): Parameters<ReactionExecuteParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("reaction_execute", move || {
            block_on_tool_future(async {
                let event = server.state.system_events.open_event(&p.event_id)?;
                let Some(event) = event else {
                    anyhow::bail!("event '{}' not found", p.event_id);
                };
                let reaction = server.state.system_events.get_reaction(&p.reaction).await;
                let Some(reaction) = reaction else {
                    anyhow::bail!("reaction '{}' not found", p.reaction);
                };
                let result = crate::system_events_runtime::worker::execute_reaction_once(
                    server.state.clone(),
                    &event,
                    &reaction,
                    p.force,
                )
                .await?;
                Ok(serde_json::to_string_pretty(&result)?)
            })
        })
        .await
    }

    #[tool(
        name = "reaction_deliveries",
        description = "List outbox delivery records with optional filters."
    )]
    pub(crate) async fn tool_reaction_deliveries(
        &self,
        Parameters(p): Parameters<ReactionDeliveriesParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("reaction_deliveries", move || {
            let mut records = server.state.system_events.outbox_store().load_all()?;
            if let Some(ref event_id) = p.event_id {
                records.retain(|r| r.event_id == *event_id);
            }
            if let Some(ref status) = p.status {
                records.retain(|r| {
                    let s = serde_json::to_value(&r.status)
                        .unwrap_or_default()
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    s == *status
                });
            }
            records.reverse();
            if let Some(limit) = p.limit {
                records.truncate(limit);
            }
            Ok(serde_json::to_string_pretty(&records)?)
        })
        .await
    }

    #[tool(
        name = "reaction_retry",
        description = "Retry a dead-lettered outbox record. Ops-only. Requires explicit outbox id."
    )]
    pub(crate) async fn tool_reaction_retry(
        &self,
        Parameters(p): Parameters<ReactionRetryParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("reaction_retry", move || {
            let found = server
                .state
                .system_events
                .outbox_store()
                .retry_dead_lettered(&p.outbox_id)?;
            if found {
                Ok(format!("requeued {}", p.outbox_id))
            } else {
                anyhow::bail!(
                    "outbox record '{}' not found or not dead-lettered",
                    p.outbox_id
                )
            }
        })
        .await
    }

    #[tool(
        name = "identity_list",
        description = "List all durable external identity mappings. Readonly."
    )]
    pub(crate) fn tool_identity_list(
        &self,
        Parameters(_p): Parameters<IdentityListParams>,
    ) -> CallToolResult {
        Self::run("identity_list", || {
            let ids = self.state.system_events.identity_registry().list_all();
            Ok(serde_json::to_string_pretty(&ids)?)
        })
    }

    #[tool(
        name = "identity_get",
        description = "Get a single external identity mapping by (scope, instance, subject, provider, model). Readonly."
    )]
    pub(crate) fn tool_identity_get(
        &self,
        Parameters(p): Parameters<IdentityGetParams>,
    ) -> CallToolResult {
        Self::run("identity_get", || {
            let reg = self.state.system_events.identity_registry();
            match reg.lookup(&p.scope, &p.instance, &p.subject, &p.provider, &p.model) {
                Some(id) => Ok(serde_json::to_string_pretty(&id)?),
                None => anyhow::bail!(
                    "identity not found: scope={} instance={} subject={} provider={} model={}",
                    p.scope,
                    p.instance,
                    p.subject,
                    p.provider,
                    p.model
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;

    fn test_hub() -> (Arc<system_events::EventHub>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            Arc::new(system_events::EventHub::new(
                system_events::EventStore::new_at(dir.path().join("journal")),
                system_events::OutboxStore::new(dir.path().join("outbox")).unwrap(),
                dir.path().join("reactions"),
                dir.path().join("identities"),
            )),
            dir,
        )
    }

    #[tokio::test]
    async fn event_catalog_cursor_survives_new_appends_and_omits_large_payloads() {
        let (hub, _dir) = test_hub();
        let mut ids = Vec::new();
        for _ in 0..25 {
            let outcome = hub
                .emit(system_events::SystemEventDraft {
                    kind: system_events::types::SystemEventKind::TaskStarted,
                    producer: "catalog-test".into(),
                    project: Some("/private/daemon/project".into()),
                    principal: None,
                    subject: None,
                    correlation: serde_json::Map::new(),
                    causation_id: None,
                    payload: json!({"secret": "synthetic-secret", "body": "界".repeat(10000)}),
                })
                .await
                .unwrap();
            ids.push(outcome.event.id);
        }
        let first = hub
            .list_event_page(None, None, None, Some("catalog-test"), None)
            .unwrap();
        assert_eq!(first["count"], 20);
        assert_eq!(first["events"][0]["id"], ids[24]);
        assert!(!first.to_string().contains("synthetic-secret"));
        assert!(!first.to_string().contains("/private/daemon"));
        let cursor = first["next_before"].as_str().unwrap();
        hub.emit(system_events::SystemEventDraft {
            kind: system_events::types::SystemEventKind::TaskStarted,
            producer: "catalog-test".into(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({}),
        })
        .await
        .unwrap();
        let next = hub
            .list_event_page(None, Some(cursor), None, Some("catalog-test"), None)
            .unwrap();
        assert_eq!(next["count"], 5);
        assert_eq!(next["events"][0]["id"], ids[4]);
        assert_eq!(next["events"][4]["id"], ids[0]);
        assert!(next["next_before"].is_null());
        assert!(
            hub.list_event_page(None, Some("missing"), None, None, None)
                .unwrap_err()
                .to_string()
                .contains("event_cursor_unavailable")
        );
        let opened = hub.open_event(&ids[0]).unwrap().unwrap();
        assert_eq!(opened.payload["secret"], "synthetic-secret");
    }

    #[test]
    fn reaction_catalog_pages_separate_safe_warnings_and_never_expand_arguments() {
        let mut spec = valid_reaction_spec("example");
        spec.action = system_events::types::ReactionAction::HttpJson {
            args: json!({"url": "https://example.test/synthetic-secret", "headers": {"Authorization": "synthetic-secret"}}),
        };
        spec.idempotency_key = Some("synthetic-secret".into());
        spec.when = Some("synthetic-secret".into());
        let result = system_events::hub::ReactionListResult {
            reactions: vec![spec],
            warnings: (0..30)
                .map(|i| system_events::hub::ReactionLoadWarning {
                    source: format!("/private/daemon/reactions/example-{i:02}.json"),
                    reason: "parse error: synthetic-secret".into(),
                })
                .collect(),
        };
        for detail in [false, true] {
            let p: ReactionListParams = serde_json::from_value(json!({"detail": detail})).unwrap();
            let page = reaction_catalog_page(result.clone(), &p).unwrap();
            assert_eq!(page["warning_count"], 30);
            assert!(page.get("warnings").is_none());
            assert_eq!(page["reactions"][0]["action"], "http_json");
            assert!(!page.to_string().contains("synthetic-secret"));
            assert!(!page.to_string().contains("/private/daemon"));
        }
        let p: ReactionListParams = serde_json::from_value(json!({"view": "warnings"})).unwrap();
        let page = reaction_catalog_page(result.clone(), &p).unwrap();
        assert_eq!(page["count"], 20);
        assert_eq!(page["next_offset"], 20);
        assert_eq!(page["warnings"][0]["name"], "example-00");
        assert_eq!(page["warnings"][0]["reason"], "invalid_json");
        assert!(!page.to_string().contains("synthetic-secret"));
        assert!(!page.to_string().contains("/private/daemon"));
        let p: ReactionListParams =
            serde_json::from_value(json!({"view": "warnings", "offset": 20})).unwrap();
        let next = reaction_catalog_page(result, &p).unwrap();
        assert_eq!(next["count"], 10);
        assert_eq!(next["warnings"][0]["name"], "example-20");
        assert!(next["next_offset"].is_null());
    }

    #[tokio::test]
    async fn tool_emit_via_hub_appends_and_returns() {
        let (hub, _dir) = test_hub();
        let draft = system_events::SystemEventDraft {
            kind: system_events::types::SystemEventKind::TaskStarted,
            producer: "test-tool".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({"note": "synthetic"}),
        };
        let outcome = hub.emit(draft).await.unwrap();
        assert!(outcome.event.id.starts_with("evt-"));
        assert!(outcome.journal_appended);
    }

    #[tokio::test]
    async fn tool_list_via_hub_returns_emitted_events() {
        let (hub, _dir) = test_hub();
        hub.emit(system_events::SystemEventDraft {
            kind: system_events::types::SystemEventKind::TaskCompleted,
            producer: "list-test".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({}),
        })
        .await
        .unwrap();

        let events = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].producer, "list-test");
    }

    #[tokio::test]
    async fn tool_open_via_hub_returns_event_with_links() {
        let (hub, _dir) = test_hub();
        let outcome = hub
            .emit(system_events::SystemEventDraft {
                kind: system_events::types::SystemEventKind::TaskStarted,
                producer: "open-test".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();

        let event = hub.open_event(&outcome.event.id).unwrap().unwrap();
        assert_eq!(event.id, outcome.event.id);

        let chain = hub.causation_chain_for(&outcome.event.id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, outcome.event.id);

        let derived = hub.derived_events(&outcome.event.id).unwrap();
        assert!(derived.is_empty());
    }

    fn valid_reaction_spec(name: &str) -> system_events::types::ReactionSpec {
        system_events::types::ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: name.to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("key:${event.id}".to_string()),
            action: system_events::types::ReactionAction::EmitEvent {
                args: json!({"kind": "derived.event"}),
            },
            retry: system_events::types::RetryPolicy::default(),
            on_failure: system_events::types::FailurePolicy::DeadLetter,
        }
    }

    #[tokio::test]
    async fn reaction_install_rejects_invalid_spec() {
        let (hub, _dir) = test_hub();
        let mut spec = valid_reaction_spec("bad");
        spec.contract = "wrong/v1".to_string();
        let result = hub.install_reaction(spec, false).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unsupported contract")
        );
    }

    #[tokio::test]
    async fn reaction_install_requires_replace_when_name_exists() {
        let (hub, _dir) = test_hub();
        hub.install_reaction(valid_reaction_spec("dup-test"), false)
            .await
            .unwrap();
        let result = hub
            .install_reaction(valid_reaction_spec("dup-test"), false)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        hub.install_reaction(valid_reaction_spec("dup-test"), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reaction_install_persists_to_disk() {
        let (hub, _dir) = test_hub();
        hub.install_reaction(valid_reaction_spec("persist-test"), false)
            .await
            .unwrap();

        let reactions_dir = hub.reactions_dir().to_path_buf();
        let path = reactions_dir.join("persist-test.json");
        assert!(path.exists(), "reaction file should exist on disk");
        let loaded: system_events::types::ReactionSpec =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.name, "persist-test");
    }

    #[test]
    fn reaction_install_schema_uses_explicit_spec_object() {
        let schema =
            serde_json::to_value(rmcp::schemars::schema_for!(ReactionInstallParams)).unwrap();
        let spec_schema = &schema["properties"]["spec"];
        assert!(
            spec_schema.is_object(),
            "spec schema must be an explicit schema object: {spec_schema}"
        );
        assert!(
            !spec_schema.is_boolean(),
            "MCP clients reject boolean subschemas for tool input properties"
        );
    }

    #[tokio::test]
    async fn dry_run_renders_event_payload_instance() {
        let (hub, _dir) = test_hub();
        let outcome = hub
            .emit(system_events::SystemEventDraft {
                kind: system_events::types::SystemEventKind::TaskCompleted,
                producer: "dry-run-test".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({"instance": "forgejo-bro-reviewer"}),
            })
            .await
            .unwrap();

        let spec = system_events::types::ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "payload-render-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("${event.payload.instance}".to_string()),
            action: system_events::types::ReactionAction::EmitEvent {
                args: json!({"kind": "derived.event", "ref": "${event.payload.instance}"}),
            },
            retry: system_events::types::RetryPolicy::default(),
            on_failure: system_events::types::FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        let reaction = hub.get_reaction("payload-render-test").await.unwrap();
        let event = hub.open_event(&outcome.event.id).unwrap().unwrap();
        let outbox = hub.outbox_store().load_all().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        let result = system_events::dry_run_replay(&reaction, &event, &packets, &outbox).unwrap();

        assert_eq!(
            result.rendered_idempotency_key,
            Some("forgejo-bro-reviewer".to_string())
        );
        let args = result.rendered_action_args.as_object().unwrap();
        assert_eq!(args["ref"], "forgejo-bro-reviewer");
    }

    #[tokio::test]
    async fn dry_run_unresolved_template_hard_errors() {
        let (hub, _dir) = test_hub();
        let outcome = hub
            .emit(system_events::SystemEventDraft {
                kind: system_events::types::SystemEventKind::TaskCompleted,
                producer: "test".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();

        let spec = system_events::types::ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "unresolved-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("${event.nonexistent.field}".to_string()),
            action: system_events::types::ReactionAction::EmitEvent { args: json!({}) },
            retry: system_events::types::RetryPolicy::default(),
            on_failure: system_events::types::FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        let reaction = hub.get_reaction("unresolved-test").await.unwrap();
        let event = hub.open_event(&outcome.event.id).unwrap().unwrap();
        let outbox = hub.outbox_store().load_all().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        let result = system_events::dry_run_replay(&reaction, &event, &packets, &outbox);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unresolved"));
    }

    #[tokio::test]
    async fn dry_run_redacts_authorization_and_secret_refs() {
        let (hub, _dir) = test_hub();
        let outcome = hub
            .emit(system_events::SystemEventDraft {
                kind: system_events::types::SystemEventKind::TaskCompleted,
                producer: "redact-test".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();

        let spec = system_events::types::ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "redact-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("static-key".to_string()),
            action: system_events::types::ReactionAction::HttpJson {
                args: json!({
                    "url": "https://example.com",
                    "headers": {
                        "Authorization": "Bearer secret:forgejo-admin-token",
                        "X-Custom": "safe-value"
                    },
                    "body": {
                        "token": "secret:another-secret"
                    }
                }),
            },
            retry: system_events::types::RetryPolicy::default(),
            on_failure: system_events::types::FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        let reaction = hub.get_reaction("redact-test").await.unwrap();
        let event = hub.open_event(&outcome.event.id).unwrap().unwrap();
        let outbox = hub.outbox_store().load_all().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        let result = system_events::dry_run_replay(&reaction, &event, &packets, &outbox).unwrap();

        let args_str = serde_json::to_string(&result.rendered_action_args).unwrap();
        assert!(
            args_str.contains("[REDACTED]"),
            "should contain redacted values"
        );
        assert!(
            !args_str.contains("secret:forgejo-admin-token"),
            "secret ref should be redacted"
        );
        assert!(
            !args_str.contains("secret:another-secret"),
            "secret ref should be redacted"
        );
        assert!(
            !args_str.contains("Bearer"),
            "authorization should be redacted"
        );
        assert!(
            args_str.contains("safe-value"),
            "non-secret values should pass through"
        );
    }

    #[tokio::test]
    async fn dry_run_does_not_write_outbox_records() {
        let (hub, _dir) = test_hub();
        let outcome = hub
            .emit(system_events::SystemEventDraft {
                kind: system_events::types::SystemEventKind::TaskCompleted,
                producer: "no-write-test".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation: serde_json::Map::new(),
                causation_id: None,
                payload: json!({}),
            })
            .await
            .unwrap();

        let before_count = hub.outbox_store().load_all().unwrap().len();

        let reaction = hub.get_reaction("redact-test").await;
        assert!(reaction.is_none(), "no reaction installed for this test");

        let spec = system_events::types::ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "no-write-reaction".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("key".to_string()),
            action: system_events::types::ReactionAction::EmitEvent { args: json!({}) },
            retry: system_events::types::RetryPolicy::default(),
            on_failure: system_events::types::FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();
        let reaction = hub.get_reaction("no-write-reaction").await.unwrap();
        let event = hub.open_event(&outcome.event.id).unwrap().unwrap();
        let outbox = hub.outbox_store().load_all().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();

        let _ = system_events::dry_run_replay(&reaction, &event, &packets, &outbox).unwrap();

        let after_count = hub.outbox_store().load_all().unwrap().len();
        assert_eq!(
            before_count, after_count,
            "dry-run must not write outbox records"
        );
    }

    #[tokio::test]
    async fn reaction_list_returns_warnings_shape() {
        let (hub, _dir) = test_hub();
        hub.install_reaction(valid_reaction_spec("list-test"), false)
            .await
            .unwrap();

        let result = hub.list_reactions_with_warnings().await;
        assert_eq!(result.reactions.len(), 1);
        assert_eq!(result.reactions[0].name, "list-test");
        assert!(
            result.warnings.is_empty(),
            "no warnings expected for valid spec"
        );

        let bad_path = hub.reactions_dir().join("broken.json");
        std::fs::write(&bad_path, "not json").unwrap();

        let result_with_warnings = hub.list_reactions_with_warnings().await;
        assert_eq!(result_with_warnings.reactions.len(), 1);
        assert_eq!(result_with_warnings.warnings.len(), 1);
        assert!(
            result_with_warnings.warnings[0]
                .reason
                .contains("parse error"),
            "expected parse error warning"
        );
    }

    #[test]
    fn deliveries_returns_outbox_records() {
        let (hub, _dir) = test_hub();
        hub.outbox_store()
            .create_record("evt-1", "react-a", Some("key-a".to_string()))
            .unwrap();
        hub.outbox_store()
            .create_record("evt-2", "react-b", None)
            .unwrap();

        let all = hub.outbox_store().load_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn deliveries_filter_by_event_id() {
        let (hub, _dir) = test_hub();
        hub.outbox_store()
            .create_record("evt-1", "react-a", Some("key-a".to_string()))
            .unwrap();
        hub.outbox_store()
            .create_record("evt-2", "react-b", None)
            .unwrap();

        let mut records = hub.outbox_store().load_all().unwrap();
        records.retain(|r| r.event_id == "evt-1");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_id, "evt-1");
    }

    #[test]
    fn deliveries_filter_by_status() {
        let (hub, _dir) = test_hub();
        let rec = hub
            .outbox_store()
            .create_record("evt-1", "react", Some("key".to_string()))
            .unwrap();
        hub.outbox_store()
            .mark_dead_lettered(&rec.id, "test", None)
            .unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let dead: Vec<_> = records
            .iter()
            .filter(|r| {
                serde_json::to_value(&r.status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("")
                    == "dead_lettered"
            })
            .collect();
        assert_eq!(dead.len(), 1);
    }

    #[test]
    fn retry_requeues_dead_lettered_record() {
        let (hub, _dir) = test_hub();
        let rec = hub
            .outbox_store()
            .create_record("evt-1", "react", Some("key".to_string()))
            .unwrap();
        hub.outbox_store()
            .mark_dead_lettered(&rec.id, "test reason", None)
            .unwrap();

        let found = hub.outbox_store().retry_dead_lettered(&rec.id).unwrap();
        assert!(found);

        let loaded = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(loaded.status, system_events::types::OutboxStatus::Pending);
        assert!(loaded.dead_letter_reason.is_none());
    }

    #[test]
    fn retry_rejects_non_dead_lettered() {
        let (hub, _dir) = test_hub();
        let rec = hub
            .outbox_store()
            .create_record("evt-1", "react", Some("key".to_string()))
            .unwrap();

        let found = hub.outbox_store().retry_dead_lettered(&rec.id).unwrap();
        assert!(!found, "pending record should not be retryable");
    }

    #[test]
    fn retry_rejects_unknown_id() {
        let (hub, _dir) = test_hub();
        let found = hub
            .outbox_store()
            .retry_dead_lettered("nonexistent")
            .unwrap();
        assert!(!found);
    }

    // -----------------------------------------------------------------------
    // Phase 8 follow-up: synthetic emit accepts principal/subject/correlation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn synthetic_emit_via_params_persists_principal_subject_correlation() {
        // Deserialize SystemEventEmitParams from the exact JSON shape an MCP
        // client would send, route through draft_from_emit_params (the helper
        // tool_system_event_emit uses), then emit. This exercises the params
        // → draft path that the prior test bypassed.
        let raw_params = json!({
            "kind": "bro.identity.required",
            "producer": "manual-test",
            "project": "/repo/test",
            "principal": {
                "kind": "bro",
                "bro": "keystone-review",
                "provider": "claude",
                "model": "haiku-4.5",
                "effort": "medium"
            },
            "subject": {
                "kind": "bro",
                "id": "bro:keystone-review"
            },
            "correlation": {
                "arc_id": "arc-test-123"
            },
            "payload": {"identity_scope": "forgejo"}
        });
        let params: SystemEventEmitParams =
            serde_json::from_value(raw_params).expect("SystemEventEmitParams should deserialize");

        let draft =
            draft_from_emit_params(params).expect("draft_from_emit_params must accept the params");

        let (hub, _dir) = test_hub();
        let outcome = hub.emit(draft).await.unwrap();
        let event_id = outcome.event.id.clone();
        let opened = hub.open_event(&event_id).unwrap().expect("event present");

        assert!(matches!(
            opened.kind,
            system_events::types::SystemEventKind::BroIdentityRequired
        ));
        assert_eq!(opened.producer, "manual-test");
        assert_eq!(opened.project.as_deref(), Some("/repo/test"));
        let principal = opened.principal.as_ref().expect("principal preserved");
        assert_eq!(principal.kind, "bro");
        assert_eq!(principal.bro.as_deref(), Some("keystone-review"));
        assert_eq!(principal.provider.as_deref(), Some("claude"));
        assert_eq!(principal.model.as_deref(), Some("haiku-4.5"));
        assert_eq!(principal.effort.as_deref(), Some("medium"));
        let subject = opened.subject.as_ref().expect("subject preserved");
        assert_eq!(subject.kind, "bro");
        assert_eq!(subject.id, "bro:keystone-review");
        assert_eq!(
            opened.correlation.get("arc_id"),
            Some(&json!("arc-test-123"))
        );
        assert_eq!(
            opened
                .payload
                .get("identity_scope")
                .and_then(|v| v.as_str()),
            Some("forgejo")
        );
    }

    #[test]
    fn draft_from_emit_params_rejects_malformed_subject() {
        // subject missing required `id` — EventSubject has both kind and id required.
        let params: SystemEventEmitParams = serde_json::from_value(json!({
            "kind": "task.started",
            "producer": "test",
            "subject": {"kind": "bro"},
            "payload": {}
        }))
        .unwrap();
        let err = draft_from_emit_params(params).unwrap_err().to_string();
        assert!(
            err.contains("invalid subject"),
            "error must call out subject: {err}"
        );
    }

    #[test]
    fn draft_from_emit_params_rejects_malformed_principal() {
        // principal must be an object, not a scalar.
        let params: SystemEventEmitParams = serde_json::from_value(json!({
            "kind": "task.started",
            "producer": "test",
            "principal": "not-an-object",
            "payload": {}
        }))
        .unwrap();
        let err = draft_from_emit_params(params).unwrap_err().to_string();
        assert!(
            err.contains("invalid principal"),
            "error must call out principal: {err}"
        );
    }

    #[test]
    fn draft_from_emit_params_defaults_correlation_to_empty() {
        let params: SystemEventEmitParams = serde_json::from_value(json!({
            "kind": "task.started",
            "producer": "test",
            "payload": {}
        }))
        .unwrap();
        let draft = draft_from_emit_params(params).unwrap();
        assert!(draft.principal.is_none());
        assert!(draft.subject.is_none());
        assert!(draft.correlation.is_empty());
    }

    // -----------------------------------------------------------------------
    // Phase 8 follow-up: dry-run example reaction against synthetic emit
    // produces a fully-resolved idempotency key (no null segments).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn example_forgejo_reaction_dry_run_renders_full_idempotency_key() {
        // Construct the synthetic emit from the same JSON shape an operator
        // would pass to `system_event_emit`, route through the helper so
        // this test exercises the params → draft → emit path the README
        // documents.
        let params: SystemEventEmitParams = serde_json::from_value(json!({
            "kind": "bro.identity.required",
            "producer": "manual-test",
            "principal": {
                "kind": "bro",
                "bro": "keystone-review",
                "provider": "claude",
                "model": "haiku-4.5"
            },
            "subject": {"kind": "bro", "id": "bro:keystone-review"},
            "payload": {
                "identity_scope": "forgejo",
                "instance": "local-forgejo15",
                "bro": "keystone-review",
                "provider": "claude",
                "model": "haiku-4.5",
                "username": "bro-keystone-review-claude-haiku45",
                "display_name": "keystone-review / claude haiku-4.5",
                "email": "bro-keystone-review@blackbox.local",
                "owner": "keystone-admin",
                "repo": "buggy"
            }
        }))
        .unwrap();
        let draft = draft_from_emit_params(params).unwrap();

        let (hub, _dir) = test_hub();
        let outcome = hub.emit(draft).await.unwrap();
        let event = outcome.event.clone();

        let spec_src =
            include_str!("../../examples/system-events/reactions/forgejo-ensure-bro-user.json");
        let spec: system_events::types::ReactionSpec = serde_json::from_str(spec_src).unwrap();

        // The reaction's `when` references a gate packet; compile it so dry-run can resolve.
        let packet_src =
            include_str!("../../examples/system-events/packets/forgejo-identity-required.json");
        let packet_params: crate::packets::CompileParams =
            serde_json::from_str(packet_src).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        packets.compile(&packet_params).unwrap();

        let result = system_events::dry_run_replay(&spec, &event, &packets, &[])
            .expect("dry-run should not hard-error on the example reaction");

        let key = result
            .rendered_idempotency_key
            .expect("idempotency key must render");
        assert!(
            key.contains("bro:keystone-review"),
            "rendered key must include subject id: {key}"
        );
        assert!(
            key.contains("claude"),
            "rendered key must include provider: {key}"
        );
        assert!(
            key.contains("haiku-4.5"),
            "rendered key must include model: {key}"
        );
        assert!(
            !key.contains("null"),
            "rendered key must not contain null segments: {key}"
        );
        assert!(
            !key.contains("${"),
            "rendered key must not contain raw template heads: {key}"
        );
    }
}
