//! Convert dispatch MCP definitions to the servers an ACP session attaches.
//!
//! The protocol carries header and environment values as literals inside the
//! `session/new` request, so references to worker variables are resolved here,
//! on the execution host, and never reach argv or a log.
use anyhow::{Context, ensure};
use bro_protocol::AgySessionConfig;
use serde_json::{Value, json};

/// The session's MCP servers in protocol form. `env` looks a referenced
/// variable up in the worker environment; an unset variable is an error.
pub fn servers(
    config: &AgySessionConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Vec<Value>> {
    ensure!(config.errors.is_empty(), "{}", config.errors.join("; "));
    let mut servers = Vec::new();
    for (name, server) in &config.mcp_servers {
        ensure!(valid_name(name), "invalid MCP server name {name:?}");
        let kind = server["type"]
            .as_str()
            .unwrap_or(if server.get("url").is_some() {
                "http"
            } else {
                "stdio"
            });
        match kind {
            "http" => {
                let url = server["url"]
                    .as_str()
                    .context("MCP HTTP server requires a URL")?;
                let mut headers = Vec::new();
                if let Some(values) = server.get("headers") {
                    for (header, value) in values
                        .as_object()
                        .context("MCP headers must be an object")?
                    {
                        let value = value
                            .as_str()
                            .context("MCP header values must be strings")?;
                        let value = if header.eq_ignore_ascii_case("authorization") {
                            authorization(value, env)?
                        } else {
                            resolve(value, env)?
                        };
                        headers.push(json!({"name": header, "value": value}));
                    }
                }
                servers.push(json!({"type": "http", "name": name, "url": url, "headers": headers}));
            }
            "stdio" => {
                let command = server["command"]
                    .as_str()
                    .context("MCP stdio server requires a command")?;
                let args = match server.get("args") {
                    Some(args) => {
                        ensure!(
                            args.as_array()
                                .is_some_and(|args| args.iter().all(Value::is_string)),
                            "MCP arguments must be strings"
                        );
                        args.clone()
                    }
                    None => json!([]),
                };
                let mut variables = Vec::new();
                if let Some(values) = server.get("env") {
                    for (key, value) in values.as_object().context("MCP env must be an object")? {
                        let value = value.as_str().context("MCP env values must be strings")?;
                        variables.push(json!({"name": key, "value": resolve(value, env)?}));
                    }
                }
                servers.push(
                    json!({"name": name, "command": command, "args": args, "env": variables}),
                );
            }
            kind => anyhow::bail!("unsupported agy MCP transport {kind:?}"),
        }
    }
    Ok(servers)
}

/// A literal value, or the worker variable a whole-value reference names.
fn resolve(value: &str, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    if let Some(variable) = reference(value) {
        return env(variable)
            .with_context(|| format!("MCP value references unset worker variable {variable}"));
    }
    ensure!(
        !value.contains("${") && !value.contains("$env:"),
        "unsupported MCP value reference"
    );
    Ok(value.to_string())
}

/// An `Authorization` value. A whole-value reference names a bearer token.
fn authorization(value: &str, env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let token = match value.strip_prefix("Bearer ") {
        Some(rest) if reference(rest).is_some() => rest,
        _ => value,
    };
    if reference(token).is_some() {
        return Ok(format!("Bearer {}", resolve(token, env)?));
    }
    resolve(value, env)
}

fn reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("$env:")
        .or_else(|| value.strip_prefix("${").and_then(|s| s.strip_suffix('}')))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(server: Value) -> AgySessionConfig {
        AgySessionConfig {
            mcp_servers: BTreeMap::from([("test".into(), server)]),
            ..Default::default()
        }
    }

    fn env(name: &str) -> Option<String> {
        (name == "TOKEN").then(|| "secret".to_string())
    }

    #[test]
    fn http_headers_resolve_worker_variables_and_bearer_tokens() {
        let native = servers(
            &config(json!({
                "type": "http",
                "url": "https://example.test/mcp",
                "headers": {"Authorization": "$env:TOKEN", "x-binding": "${TOKEN}", "x-plain": "v"},
                "exclude_tools": [],
            })),
            &env,
        )
        .unwrap();
        assert_eq!(
            native,
            vec![json!({
                "type": "http",
                "name": "test",
                "url": "https://example.test/mcp",
                "headers": [
                    {"name": "Authorization", "value": "Bearer secret"},
                    {"name": "x-binding", "value": "secret"},
                    {"name": "x-plain", "value": "v"},
                ],
            })]
        );
        let explicit = servers(
            &config(json!({"url": "https://example.test", "headers": {"authorization": "Bearer ${TOKEN}"}})),
            &env,
        )
        .unwrap();
        assert_eq!(explicit[0]["headers"][0]["value"], "Bearer secret");
        let literal = servers(
            &config(
                json!({"url": "https://example.test", "headers": {"Authorization": "Basic abc"}}),
            ),
            &env,
        )
        .unwrap();
        assert_eq!(literal[0]["headers"][0]["value"], "Basic abc");
    }

    #[test]
    fn stdio_servers_carry_command_arguments_and_environment() {
        let native = servers(
            &config(json!({"command": "tool", "args": ["--serve"], "env": {"KEY": "$env:TOKEN", "MODE": "x"}})),
            &env,
        )
        .unwrap();
        assert_eq!(
            native,
            vec![json!({
                "name": "test",
                "command": "tool",
                "args": ["--serve"],
                "env": [{"name": "KEY", "value": "secret"}, {"name": "MODE", "value": "x"}],
            })]
        );
    }

    #[test]
    fn malformed_unsupported_and_unresolved_servers_are_refused() {
        for server in [
            json!({"type": "sse", "url": "https://example.test"}),
            json!({"type": "stdio", "command": 42}),
            json!({"type": "stdio", "command": "tool", "args": [1]}),
            json!({"type": "http", "url": "https://example.test", "headers": {"X": 42}}),
            json!({"type": "http", "url": "https://example.test", "headers": {"X": "$env:UNSET"}}),
            json!({"type": "http", "url": "https://example.test", "headers": {"X": "a ${TOKEN} b"}}),
            json!({"type": "http"}),
        ] {
            assert!(servers(&config(server.clone()), &env).is_err(), "{server}");
        }
        let named = AgySessionConfig {
            mcp_servers: BTreeMap::from([("bad name".into(), json!({"command": "tool"}))]),
            ..Default::default()
        };
        assert!(servers(&named, &env).is_err());
        let composed = AgySessionConfig {
            errors: vec!["agy does not support service_tier".into()],
            ..Default::default()
        };
        assert!(servers(&composed, &env).is_err());
    }
}
