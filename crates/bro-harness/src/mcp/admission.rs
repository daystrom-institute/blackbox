//! Session startup admission. Connections and catalogs stay fixed until close.

use super::*;
use anyhow::{Context, Result, ensure};
use std::collections::HashSet;

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerReadiness {
    pub server: String,
    pub required: bool,
    pub status: &'static str,
    pub tool_count: usize,
    pub catalog: &'static str,
    pub remote_tool_timeout_ms: Option<u64>,
}

#[derive(Default)]
pub struct McpLoad {
    pub tools: Vec<Arc<dyn Tool>>,
    pub readiness: Vec<McpServerReadiness>,
}

/// Parse and admit configured servers. Malformed config never becomes an empty
/// successful catalog. Optional connection failures have sanitized readiness.
pub async fn load_mcp_tools(config: Option<&str>, filter: &ToolFilter) -> Result<McpLoad> {
    load_mcp_tools_with_capability_aliases(config, filter, None).await
}

pub async fn load_mcp_tools_with_capability_aliases(
    config: Option<&str>,
    filter: &ToolFilter,
    capability_server: Option<&str>,
) -> Result<McpLoad> {
    let Some(config) = config else {
        return Ok(McpLoad::default());
    };
    load_mcp_tools_from_config_with_capability_aliases(
        &McpConfig::from_json(config)?,
        filter,
        capability_server,
    )
    .await
}

pub async fn load_mcp_tools_from_config(
    config: &McpConfig,
    filter: &ToolFilter,
) -> Result<McpLoad> {
    load_mcp_tools_from_config_with_capability_aliases(config, filter, None).await
}

pub async fn load_mcp_tools_from_config_with_capability_aliases(
    config: &McpConfig,
    filter: &ToolFilter,
    capability_server: Option<&str>,
) -> Result<McpLoad> {
    validate_config(config)?;
    let mut loaded = McpLoad::default();
    let mut canonical_names = HashSet::new();
    let mut javascript_names = HashSet::new();
    for server in &config.servers {
        let policy = config
            .server_policies
            .get(server.name())
            .cloned()
            .unwrap_or_default();
        let connection = tokio::time::timeout(
            std::time::Duration::from_millis(policy.startup_timeout_ms),
            server_backend_and_specs(server, policy.tool_timeout_ms),
        )
        .await;
        let (backend, specs) = match connection {
            Ok(Ok(connection)) => connection,
            unavailable => {
                let status = if unavailable.is_err() {
                    "timed_out"
                } else {
                    "unavailable"
                };
                // Connection error strings can contain credentials, URLs, paths,
                // or server-controlled messages. Publish only a classified state.
                if policy.required {
                    anyhow::bail!("required MCP server {} is {status}", server.name());
                }
                loaded.readiness.push(McpServerReadiness {
                    server: server.name().to_owned(),
                    required: false,
                    status,
                    tool_count: 0,
                    catalog: "fixed_for_session",
                    remote_tool_timeout_ms: (!matches!(server, McpServerConfig::InProcess { .. }))
                        .then_some(policy.tool_timeout_ms),
                });
                continue;
            }
        };
        let mut local_names = HashSet::new();
        let before = loaded.tools.len();
        for spec in specs {
            config::validate_component(&spec.name).context("invalid MCP tool name")?;
            ensure!(
                local_names.insert(spec.name.clone()),
                "MCP server returned duplicate tool names"
            );
            ensure!(
                spec.input_schema.is_object(),
                "MCP tool input schema must be an object"
            );
            ensure!(
                spec.output_schema.as_ref().is_none_or(Value::is_object),
                "MCP tool output schema must be an object"
            );
            let qualified = format!("mcp__{}__{}", server.name(), spec.name);
            let excluded = server.excludes(&spec.name, &qualified)
                || policy.exclude_tools.iter().any(|pattern| {
                    pattern_matches(pattern, &spec.name) || pattern_matches(pattern, &qualified)
                });
            if excluded {
                continue;
            }
            let mut names = Vec::new();
            if capability_server == Some(server.name())
                && !filter.denied(&qualified)
                && let Some(alias) = capability_alias(&spec.name)
                && filter.permits(alias)
            {
                names.push(alias.to_owned());
            }
            if filter.permits(&qualified) {
                names.push(qualified);
            }
            for name in names {
                ensure!(
                    canonical_names.insert(name.clone()),
                    "MCP catalog has colliding canonical tool names"
                );
                ensure!(
                    javascript_names.insert(bro_code_mode::normalize_code_mode_identifier(&name)),
                    "MCP catalog has colliding JavaScript tool names"
                );
                let title = spec.title.as_deref().or_else(|| {
                    spec.annotations
                        .as_ref()
                        .and_then(|annotations| annotations.title.as_deref())
                });
                let description = match title {
                    Some(title) => {
                        format!("{title}\n{}\n{}", spec.description, result::RESULT_GUIDANCE)
                    }
                    None => format!("{}\n{}", spec.description, result::RESULT_GUIDANCE),
                };
                loaded.tools.push(Arc::new(McpTool {
                    backend: backend.clone(),
                    call_name: spec.name.clone(),
                    name,
                    description,
                    schema: spec.input_schema.clone(),
                    output_schema: result_schema(&spec),
                    annotations: project_annotations(spec.annotations.as_ref()),
                }));
            }
        }
        loaded.readiness.push(McpServerReadiness {
            server: server.name().to_owned(),
            required: policy.required,
            status: "ready",
            tool_count: loaded.tools.len() - before,
            catalog: "fixed_for_session",
            remote_tool_timeout_ms: (!matches!(server, McpServerConfig::InProcess { .. }))
                .then_some(policy.tool_timeout_ms),
        });
    }
    Ok(loaded)
}

pub(super) fn validate_config(config: &McpConfig) -> Result<()> {
    let mut names = HashSet::new();
    for server in &config.servers {
        config::validate_component(server.name())?;
        ensure!(
            names.insert(server.name()),
            "duplicate configured MCP server name"
        );
        match server {
            McpServerConfig::Http { url, headers, .. } => {
                config::validate_url(url)?;
                for (name, value) in headers {
                    HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| anyhow::anyhow!("invalid MCP header name"))?;
                    if let Some(variable) = value.strip_prefix("$env:") {
                        ensure!(
                            !variable.is_empty(),
                            "empty MCP header environment reference"
                        );
                    } else {
                        HeaderValue::from_str(value)
                            .map_err(|_| anyhow::anyhow!("invalid MCP header value"))?;
                    }
                }
            }
            McpServerConfig::Sse { .. } => anyhow::bail!(
                "legacy SSE MCP transport is unsupported; use a Streamable HTTP endpoint"
            ),
            McpServerConfig::Stdio { command, .. } => ensure!(
                !command.trim().is_empty(),
                "MCP stdio command must not be empty"
            ),
            McpServerConfig::InProcess { .. } => {}
        }
    }
    for (name, policy) in &config.server_policies {
        ensure!(
            names.contains(name.as_str()),
            "MCP policy names an unconfigured server"
        );
        config::validate_policy(policy)?;
    }
    Ok(())
}

pub(super) fn project_annotations(
    annotations: Option<&rmcp::model::ToolAnnotations>,
) -> bro_tools::ToolAnnotations {
    let read_only = annotations
        .and_then(|annotations| annotations.read_only_hint)
        .unwrap_or(false);
    let destructive = !read_only
        && annotations
            .and_then(|annotations| annotations.destructive_hint)
            .unwrap_or(true);
    bro_tools::ToolAnnotations {
        read_only,
        destructive,
    }
}

pub(super) fn result_schema(spec: &McpToolSpec) -> Value {
    serde_json::json!({
        "type":"object",
        "properties":{
            "content":{"type":"array","items":{"type":"object"}},
            "structuredContent":spec.output_schema.clone().unwrap_or_else(|| serde_json::json!({})),
            "isError":{"type":"boolean"},
            "harnessPresentation":{"type":"object"}
        },
        "required":["content","isError"],
        "x-mcp":{"title":spec.title,"annotations":spec.annotations,"outputSchema":spec.output_schema}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        tools: Vec<McpToolSpec>,
        blocked: bool,
        fails: bool,
    }

    #[async_trait]
    impl McpSurface for Fixture {
        async fn list_tools(&self) -> Result<Vec<McpToolSpec>> {
            if self.blocked {
                std::future::pending::<()>().await;
            }
            if self.fails {
                anyhow::bail!(
                    "synthetic-secret-token https://fixture.invalid/private?token=synthetic-secret-token"
                );
            }
            Ok(self.tools.clone())
        }
        async fn call_tool(&self, _: &str, _: Value) -> Result<ToolResult> {
            Ok(ToolResult::Json(json!({"count":1})))
        }
    }

    fn spec(name: &str) -> McpToolSpec {
        McpToolSpec {
            name: name.into(),
            description: "Fixture tool".into(),
            input_schema: json!({"type":"object"}),
            ..Default::default()
        }
    }

    fn fixture(tools: Vec<McpToolSpec>, blocked: bool, fails: bool) -> McpConfig {
        McpConfig {
            servers: vec![McpServerConfig::InProcess {
                name: "fixture".into(),
                server: Arc::new(Fixture {
                    tools,
                    blocked,
                    fails,
                }),
            }],
            tool_placement: BTreeMap::new(),
            server_policies: BTreeMap::new(),
        }
    }

    #[test]
    fn malformed_config_and_misleading_sse_are_rejected() {
        for config in [
            json!([]),
            json!({}),
            json!({"mcpServers":[]}),
            json!({"mcpServers":{"fixture":null}}),
            json!({"mcpServers":{"fixture":{"url":42}}}),
            json!({"mcpServers":{"fixture":{"type":"sse","url":"https://fixture.invalid/mcp"}}}),
            json!({"mcpServers":{"fixture":{"command":"fixture","args":[42]}}}),
            json!({"mcpServers":{"fixture":{"command":"fixture","env":{"KEY":false}}}}),
            json!({"mcpServers":{"fixture":{"command":"fixture","required":"yes"}}}),
            json!({"mcpServers":{"fixture":{"command":"fixture","startup_timeout_ms":0}}}),
            json!({"mcpServers":{"fixture":{"command":"fixture","startup_timeout_ms":300001}}}),
            json!({"mcpServers":{"fixture":{"command":"fixture","typo":true}}}),
            json!({"mcpServers":{"fixture":{"url":"https://fixture.invalid/mcp","headers":{"bad header":"synthetic"}}}}),
            json!({"mcpServers":{},"tool_placement":{"tool":"typo"}}),
        ] {
            assert!(
                McpConfig::from_json(&config.to_string()).is_err(),
                "accepted malformed fixture {config}"
            );
        }
    }

    #[tokio::test]
    async fn optional_failure_readiness_is_sanitized_and_required_failure_aborts() {
        let mut config = fixture(Vec::new(), false, true);
        let loaded = load_mcp_tools_from_config(&config, &ToolFilter::default())
            .await
            .unwrap();
        assert!(loaded.tools.is_empty());
        assert_eq!(loaded.readiness[0].status, "unavailable");
        let rendered = serde_json::to_string(&loaded.readiness).unwrap();
        assert!(!rendered.contains("synthetic-secret-token"));
        assert!(!rendered.contains("https://"));
        config.server_policies.insert(
            "fixture".into(),
            McpServerPolicy {
                required: true,
                ..Default::default()
            },
        );
        let error = match load_mcp_tools_from_config(&config, &ToolFilter::default()).await {
            Ok(_) => panic!("required failure admitted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("required MCP server fixture"));
        assert!(!format!("{error:#}").contains("synthetic-secret-token"));
    }

    #[tokio::test]
    async fn blocked_startup_has_a_finite_optional_or_required_outcome() {
        let mut config = fixture(Vec::new(), true, false);
        config.server_policies.insert(
            "fixture".into(),
            McpServerPolicy {
                startup_timeout_ms: 5,
                ..Default::default()
            },
        );
        let loaded = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            load_mcp_tools_from_config(&config, &ToolFilter::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(loaded.readiness[0].status, "timed_out");
        config.server_policies.get_mut("fixture").unwrap().required = true;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                load_mcp_tools_from_config(&config, &ToolFilter::default())
            )
            .await
            .unwrap()
            .is_err()
        );
    }

    #[tokio::test]
    async fn duplicate_or_ambiguous_catalog_names_fail_before_tools_escape() {
        for tools in [
            vec![spec("same"), spec("same")],
            vec![spec("search-all"), spec("search_all")],
            vec![spec("ambiguous__name")],
        ] {
            assert!(
                load_mcp_tools_from_config(&fixture(tools, false, false), &ToolFilter::default())
                    .await
                    .is_err()
            );
        }
        let mut config = fixture(vec![spec("read")], false, false);
        config.servers.push(config.servers[0].clone());
        assert!(
            load_mcp_tools_from_config(&config, &ToolFilter::default())
                .await
                .is_err()
        );
        let config = McpConfig {
            servers: ["fixture.one", "fixture_one"]
                .into_iter()
                .map(|name| McpServerConfig::InProcess {
                    name: name.into(),
                    server: Arc::new(Fixture {
                        tools: vec![spec("read")],
                        blocked: false,
                        fails: false,
                    }),
                })
                .collect(),
            tool_placement: BTreeMap::new(),
            server_policies: BTreeMap::new(),
        };
        assert!(
            load_mcp_tools_from_config(&config, &ToolFilter::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn declared_metadata_and_exclusions_survive_admission() {
        let mut declared = spec("read");
        declared.title = Some("Fixture title".into());
        declared.output_schema =
            Some(json!({"type":"object","properties":{"count":{"type":"integer"}}}));
        declared.annotations = Some(serde_json::from_value(json!({"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false})).unwrap());
        let mut config = fixture(
            vec![declared.clone(), spec("excluded"), spec("unknown")],
            false,
            false,
        );
        config.server_policies.insert(
            "fixture".into(),
            McpServerPolicy {
                exclude_tools: vec!["excluded".into()],
                ..Default::default()
            },
        );
        let loaded = load_mcp_tools_from_config(&config, &ToolFilter::default())
            .await
            .unwrap();
        assert_eq!(loaded.tools.len(), 2);
        assert!(loaded.tools[0].annotations().read_only);
        assert!(!loaded.tools[0].annotations().destructive);
        assert!(!loaded.tools[1].annotations().read_only);
        assert!(loaded.tools[1].annotations().destructive);
        assert!(loaded.tools[0].description().contains("Fixture title"));
        let output = loaded.tools[0].output_schema().unwrap();
        assert_eq!(
            output["properties"]["structuredContent"],
            declared.output_schema.unwrap()
        );
        assert_eq!(output["x-mcp"]["annotations"]["idempotentHint"], true);
        assert_eq!(loaded.readiness[0].catalog, "fixed_for_session");
        assert_eq!(loaded.readiness[0].tool_count, 2);
        let stdio = McpConfig::from_json(
            r#"{"mcpServers":{"fixture":{"command":"fixture","exclude_tools":["hidden"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            stdio.server_policies["fixture"].exclude_tools,
            vec!["hidden"]
        );
    }

    #[tokio::test]
    async fn cli_loader_propagates_parse_error_instead_of_empty_catalog() {
        assert!(
            load_mcp_tools(Some("{malformed"), &ToolFilter::default())
                .await
                .is_err()
        );
        assert!(
            load_mcp_tools(None, &ToolFilter::default())
                .await
                .unwrap()
                .tools
                .is_empty()
        );
    }
}
