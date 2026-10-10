//! `--mcp-config` and the tool filters, as app-server config overrides.
//!
//! The daemon hands every vendor CLI lane the claude CLI's MCP config shape
//! (`{"mcpServers":{"<name>":{"type":…}}}`) and `mcp__<server>__<tool>` filter
//! names. The app-server takes servers as `mcp_servers.<name>` config tables,
//! passed as `-c` overrides on its command line, and filters as each server's
//! `enabled_tools` / `disabled_tools`. Secrets stay out of argv: a header
//! whose whole value is a `${VAR}` reference becomes an `env_http_headers`
//! entry (or `bearer_token_env_var` for `Authorization: Bearer ${VAR}`), so
//! the app-server reads the value from its inherited environment.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The app-server command-line overrides for one dispatch.
#[derive(Debug, Default, PartialEq)]
pub struct McpOverrides {
    /// `-c` values, each `key=<toml value>`.
    pub config_overrides: Vec<String>,
    /// The servers this dispatch defines, sorted.
    pub servers: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct ToolFilter {
    enabled: Vec<String>,
    disabled: Vec<String>,
}

pub fn build(
    mcp_configs: &[String],
    allowed_tools: &[String],
    disallowed_tools: &[String],
) -> McpOverrides {
    let mut out = McpOverrides::default();
    let mut servers: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for raw in mcp_configs {
        let parsed: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => {
                out.warnings
                    .push(format!("ignoring --mcp-config that is not JSON: {error}"));
                continue;
            }
        };
        let Some(entries) = parsed.get("mcpServers").and_then(Value::as_object) else {
            out.warnings
                .push("ignoring --mcp-config without an mcpServers object".to_string());
            continue;
        };
        for (name, server) in entries {
            if !valid_server_name(name) {
                out.warnings.push(format!(
                    "ignoring MCP server {name:?}: the app-server needs [A-Za-z0-9_-] names"
                ));
                continue;
            }
            match server_table(name, server, &mut out.warnings) {
                Some(table) => {
                    servers.insert(name.clone(), table);
                }
                None => continue,
            }
        }
    }

    let mut filters: BTreeMap<String, ToolFilter> = BTreeMap::new();
    for (tools, allow) in [(allowed_tools, true), (disallowed_tools, false)] {
        for tool in tools {
            let Some((server, name)) = tool
                .strip_prefix("mcp__")
                .and_then(|rest| rest.split_once("__"))
                .filter(|(server, name)| !server.is_empty() && !name.is_empty())
            else {
                out.warnings.push(format!(
                    "ignoring tool filter {tool:?}: only mcp__<server>__<tool> names map onto the app-server"
                ));
                continue;
            };
            if name.contains('*') || server.contains('*') {
                out.warnings.push(format!(
                    "ignoring tool filter {tool:?}: the app-server takes exact names"
                ));
                continue;
            }
            if !valid_server_name(server) {
                out.warnings.push(format!(
                    "ignoring tool filter {tool:?}: invalid server name"
                ));
                continue;
            }
            let filter = filters.entry(server.to_string()).or_default();
            let list = if allow {
                &mut filter.enabled
            } else {
                &mut filter.disabled
            };
            if !list.iter().any(|existing| existing == name) {
                list.push(name.to_string());
            }
        }
    }

    for (name, mut table) in servers {
        if let Some(filter) = filters.remove(&name) {
            if !filter.enabled.is_empty() {
                table.insert("enabled_tools".into(), string_array(&filter.enabled));
            }
            if !filter.disabled.is_empty() {
                table.insert("disabled_tools".into(), string_array(&filter.disabled));
            }
        }
        out.config_overrides.push(format!(
            "mcp_servers.{name}={}",
            toml_value(&Value::Object(table))
        ));
        out.servers.push(name);
    }
    // Filters naming a server this dispatch does not define apply to the
    // app-server's own definition of it.
    for (name, filter) in filters {
        if !filter.enabled.is_empty() {
            out.config_overrides.push(format!(
                "mcp_servers.{name}.enabled_tools={}",
                toml_value(&string_array(&filter.enabled))
            ));
        }
        if !filter.disabled.is_empty() {
            out.config_overrides.push(format!(
                "mcp_servers.{name}.disabled_tools={}",
                toml_value(&string_array(&filter.disabled))
            ));
        }
    }
    out
}

fn server_table(
    name: &str,
    server: &Value,
    warnings: &mut Vec<String>,
) -> Option<Map<String, Value>> {
    let Some(server) = server.as_object() else {
        warnings.push(format!("ignoring MCP server {name}: not an object"));
        return None;
    };
    let kind =
        server
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(if server.contains_key("url") {
                "http"
            } else {
                "stdio"
            });
    let mut table = Map::new();
    match kind {
        "http" | "sse" => {
            let Some(url) = server.get("url").and_then(Value::as_str) else {
                warnings.push(format!("ignoring MCP server {name}: no url"));
                return None;
            };
            if kind == "sse" {
                warnings.push(format!(
                    "MCP server {name}: the app-server has no SSE transport; connecting over streamable HTTP"
                ));
            }
            table.insert("url".into(), Value::String(url.to_string()));
            let mut literal = Map::new();
            let mut from_env = Map::new();
            for (header, value) in server
                .get("headers")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let Some(value) = value.as_str() else {
                    warnings.push(format!(
                        "MCP server {name}: ignoring non-string header {header}"
                    ));
                    continue;
                };
                if let Some(var) = env_reference(value) {
                    from_env.insert(header.clone(), Value::String(var.to_string()));
                } else if header.eq_ignore_ascii_case("authorization")
                    && let Some(var) = value.strip_prefix("Bearer ").and_then(env_reference)
                {
                    table.insert(
                        "bearer_token_env_var".into(),
                        Value::String(var.to_string()),
                    );
                } else if value.contains("${") {
                    warnings.push(format!(
                        "MCP server {name}: header {header} mixes text with an environment reference; dropped"
                    ));
                } else {
                    literal.insert(header.clone(), Value::String(value.to_string()));
                }
            }
            if !literal.is_empty() {
                table.insert("http_headers".into(), Value::Object(literal));
            }
            if !from_env.is_empty() {
                table.insert("env_http_headers".into(), Value::Object(from_env));
            }
        }
        "stdio" => {
            let Some(command) = server.get("command").and_then(Value::as_str) else {
                warnings.push(format!("ignoring MCP server {name}: no command"));
                return None;
            };
            table.insert("command".into(), Value::String(command.to_string()));
            if let Some(args) = server.get("args").and_then(Value::as_array) {
                let args: Vec<String> = args
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                if !args.is_empty() {
                    table.insert("args".into(), string_array(&args));
                }
            }
            let mut env = Map::new();
            let mut passthrough = Vec::new();
            for (key, value) in server
                .get("env")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let Some(value) = value.as_str() else {
                    continue;
                };
                match env_reference(value) {
                    Some(var) if var == key => passthrough.push(key.clone()),
                    Some(_) => warnings.push(format!(
                        "MCP server {name}: env {key} renames another variable; dropped"
                    )),
                    None => {
                        env.insert(key.clone(), Value::String(value.to_string()));
                    }
                }
            }
            if !env.is_empty() {
                table.insert("env".into(), Value::Object(env));
            }
            if !passthrough.is_empty() {
                table.insert("env_vars".into(), string_array(&passthrough));
            }
        }
        other => {
            warnings.push(format!(
                "ignoring MCP server {name}: unknown type {other:?}"
            ));
            return None;
        }
    }
    Some(table)
}

/// `VAR` when `value` is exactly `${VAR}`.
fn env_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        .filter(|var| !var.is_empty() && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn string_array(items: &[String]) -> Value {
    Value::Array(items.iter().cloned().map(Value::String).collect())
}

/// Render a JSON value as an inline TOML value. JSON string escapes are a
/// subset of TOML basic-string escapes, so strings and keys reuse them.
fn toml_value(value: &Value) -> String {
    match value {
        Value::String(text) => Value::String(text.clone()).to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(toml_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            sorted(map)
                .into_iter()
                .map(|(key, value)| format!(
                    "{} = {}",
                    Value::String(key.clone()),
                    toml_value(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // TOML has no null; no table built here carries one.
        Value::Null => "\"\"".to_string(),
    }
}

/// Object entries in key order, whatever map order serde_json was built with.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_stdio_servers_become_app_server_tables_without_secrets_in_argv() {
        let config = serde_json::json!({
            "mcpServers": {
                "blackbox": {
                    "type": "http",
                    "url": "http://127.0.0.1:7264/mcp?surface=default",
                    "headers": {
                        "Authorization": "${BLACKBOX_MCP_BEARER}",
                        "X-Workspace": "${BRO_WORKSPACE_BINDING}",
                        "X-Static": "fixed"
                    }
                },
                "bearer": {
                    "type": "http",
                    "url": "https://example.test/mcp",
                    "headers": {"authorization": "Bearer ${TOKEN}", "X-Mixed": "a ${B}"}
                },
                "local": {
                    "type": "stdio",
                    "command": "local-mcp",
                    "args": ["--flag", "quote\"d"],
                    "env": {"MODE": "fast", "HOME_TOKEN": "${HOME_TOKEN}"}
                }
            }
        })
        .to_string();
        let built = build(
            &[config],
            &["mcp__blackbox__bbox_search".into(), "Bash".into()],
            &[
                "mcp__blackbox__bro_exec".into(),
                "mcp__other__x".into(),
                "mcp__blackbox__bro_*".into(),
            ],
        );
        assert_eq!(built.servers, ["bearer", "blackbox", "local"]);
        assert_eq!(
            built.config_overrides,
            [
                r#"mcp_servers.bearer={"bearer_token_env_var" = "TOKEN", "url" = "https://example.test/mcp"}"#,
                r#"mcp_servers.blackbox={"disabled_tools" = ["bro_exec"], "enabled_tools" = ["bbox_search"], "env_http_headers" = {"Authorization" = "BLACKBOX_MCP_BEARER", "X-Workspace" = "BRO_WORKSPACE_BINDING"}, "http_headers" = {"X-Static" = "fixed"}, "url" = "http://127.0.0.1:7264/mcp?surface=default"}"#,
                r#"mcp_servers.local={"args" = ["--flag", "quote\"d"], "command" = "local-mcp", "env" = {"MODE" = "fast"}, "env_vars" = ["HOME_TOKEN"]}"#,
                r#"mcp_servers.other.disabled_tools=["x"]"#,
            ]
        );
        // Bash, the glob and the mixed header each produce a warning.
        assert_eq!(built.warnings.len(), 3, "{:?}", built.warnings);
    }

    #[test]
    fn malformed_configs_and_names_are_skipped_with_warnings() {
        let built = build(
            &[
                "not json".into(),
                r#"{"other": {}}"#.into(),
                r#"{"mcpServers": {"bad.name": {"url": "http://x"}, "nourl": {"type": "http"}, "odd": {"type": "ws"}}}"#.into(),
            ],
            &[],
            &[],
        );
        assert!(built.config_overrides.is_empty());
        assert!(built.servers.is_empty());
        assert_eq!(built.warnings.len(), 5, "{:?}", built.warnings);
    }

    #[test]
    fn toml_rendering_escapes_strings() {
        assert_eq!(
            toml_value(&serde_json::json!({"a b": ["x\ny", true, 3]})),
            r#"{"a b" = ["x\ny", true, 3]}"#
        );
    }
}
