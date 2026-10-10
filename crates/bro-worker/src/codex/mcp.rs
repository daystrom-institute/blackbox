//! Convert dispatch MCP definitions to native app-server configuration.
use anyhow::{Context, ensure};
use bro_protocol::CodexSessionConfig;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn overrides(config: &CodexSessionConfig) -> anyhow::Result<BTreeMap<String, Value>> {
    ensure!(config.errors.is_empty(), "{}", config.errors.join("; "));
    ensure!(
        config.allowed_tools.is_empty(),
        "Codex cannot enforce a global tool allowlist; use a restricted MCP surface and explicit MCP denials"
    );
    let mut servers = BTreeMap::new();
    for (name, server) in &config.mcp_servers {
        ensure!(valid_name(name), "invalid MCP server name {name:?}");
        let mut native = serde_json::Map::new();
        native.insert("enabled".into(), json!(true));
        match server["type"]
            .as_str()
            .unwrap_or(if server.get("url").is_some() {
                "http"
            } else {
                "stdio"
            }) {
            "http" => {
                let url = server["url"]
                    .as_str()
                    .context("MCP HTTP server requires a URL")?;
                native.insert("url".into(), json!(url));
                let mut literal = serde_json::Map::new();
                let mut env = serde_json::Map::new();
                if let Some(headers) = server.get("headers") {
                    for (header, value) in headers
                        .as_object()
                        .context("MCP headers must be an object")?
                    {
                        let value = value
                            .as_str()
                            .context("MCP header values must be strings")?;
                        if let Some(var) = reference(value) {
                            if header.eq_ignore_ascii_case("authorization") {
                                native.insert("bearer_token_env_var".into(), json!(var));
                            } else {
                                env.insert(header.clone(), json!(var));
                            }
                        } else if header.eq_ignore_ascii_case("authorization")
                            && value.starts_with("Bearer ${")
                        {
                            native.insert(
                                "bearer_token_env_var".into(),
                                json!(reference(&value[7..]).context("invalid bearer reference")?),
                            );
                        } else {
                            ensure!(
                                !value.contains("${") && !value.contains("$env:"),
                                "unsupported MCP header reference for {header}"
                            );
                            literal.insert(header.clone(), json!(value));
                        }
                    }
                }
                if !literal.is_empty() {
                    native.insert("http_headers".into(), Value::Object(literal));
                }
                if !env.is_empty() {
                    native.insert("env_http_headers".into(), Value::Object(env));
                }
            }
            "stdio" => {
                native.insert(
                    "command".into(),
                    json!(
                        server["command"]
                            .as_str()
                            .context("MCP stdio server requires a command")?
                    ),
                );
                if let Some(args) = server.get("args") {
                    ensure!(
                        args.as_array()
                            .is_some_and(|args| args.iter().all(Value::is_string)),
                        "MCP arguments must be strings"
                    );
                    native.insert("args".into(), args.clone());
                }
                let mut env = serde_json::Map::new();
                let mut passthrough = Vec::new();
                if let Some(values) = server.get("env") {
                    for (key, value) in values.as_object().context("MCP env must be an object")? {
                        let value = value.as_str().context("MCP env values must be strings")?;
                        if let Some(var) = reference(value) {
                            ensure!(var == key, "MCP env references cannot rename variables");
                            passthrough.push(key.clone());
                        } else {
                            ensure!(
                                !value.contains("${") && !value.contains("$env:"),
                                "unsupported MCP env reference"
                            );
                            env.insert(key.clone(), json!(value));
                        }
                    }
                }
                native.insert("env".into(), Value::Object(env));
                native.insert("env_vars".into(), json!(passthrough));
            }
            kind => anyhow::bail!("unsupported Codex MCP transport {kind:?}"),
        }
        servers.insert(name.clone(), native);
    }
    for tool in &config.disallowed_tools {
        let (server, tool_name) = tool.strip_prefix("mcp__").and_then(|s| s.split_once("__"))
            .context("Codex supports only exact MCP tool denials; native tool restrictions cannot be silently ignored")?;
        ensure!(
            valid_name(server) && !tool_name.is_empty() && !tool_name.contains('*'),
            "Codex requires exact MCP tool denials: {tool}"
        );
        // An undeclared server is disabled by strict config, so needs no table.
        if let Some(table) = servers.get_mut(server) {
            table
                .entry("disabled_tools".to_string())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .expect("tool array")
                .push(json!(tool_name));
        }
    }
    Ok(servers
        .into_iter()
        .map(|(name, table)| (format!("mcp_servers.{name}"), Value::Object(table)))
        .collect())
}
fn reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("$env:")
        .or_else(|| value.strip_prefix("${").and_then(|s| s.strip_suffix('}')))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_and_unsupported_servers_are_refused() {
        for server in [
            json!({"type":"sse","url":"https://example.test"}),
            json!({"type":"stdio","command":42}),
            json!({"type":"stdio","command":"tool","args":[1]}),
            json!({"type":"http","url":"https://example.test","headers":{"X":42}}),
        ] {
            let config = CodexSessionConfig {
                mcp_servers: BTreeMap::from([("test".into(), server)]),
                ..Default::default()
            };
            assert!(overrides(&config).is_err());
        }
    }

    #[test]
    fn refuses_restrictions_it_cannot_enforce() {
        for config in [
            CodexSessionConfig {
                disallowed_tools: vec!["Bash".into()],
                ..Default::default()
            },
            CodexSessionConfig {
                allowed_tools: vec!["mcp__box__read".into()],
                ..Default::default()
            },
        ] {
            assert!(overrides(&config).is_err());
        }
    }
    #[test]
    fn bearer_is_an_environment_reference_and_deny_is_native() {
        let config = CodexSessionConfig {
            mcp_servers: BTreeMap::from([(
                "box".into(),
                json!({"type":"http","url":"https://example.test/mcp","headers":{"Authorization":"$env:TOKEN","X-Workspace":"${WORKSPACE}"}}),
            )]),
            disallowed_tools: vec!["mcp__box__write".into()],
            ..Default::default()
        };
        let tables = overrides(&config).unwrap();
        assert_eq!(tables["mcp_servers.box"]["bearer_token_env_var"], "TOKEN");
        assert_eq!(
            tables["mcp_servers.box"]["env_http_headers"]["X-Workspace"],
            "WORKSPACE"
        );
        assert_eq!(
            tables["mcp_servers.box"]["disabled_tools"],
            json!(["write"])
        );
    }
}
