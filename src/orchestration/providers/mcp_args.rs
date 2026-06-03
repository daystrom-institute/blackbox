use std::collections::BTreeMap;

use serde_json::Value;

use crate::orchestration::mcp::{self, McpFilters};

use super::Provider;

impl Provider {
    #[allow(dead_code)]
    pub fn build_mcp_add_http_args(
        &self,
        name: &str,
        url: &str,
        exclude_tools: &[String],
    ) -> Option<Vec<String>> {
        self.build_mcp_add_http_args_full(name, url, exclude_tools, &BTreeMap::new(), "user")
    }

    #[allow(dead_code)]
    pub fn build_mcp_add_http_args_scoped(
        &self,
        name: &str,
        url: &str,
        exclude_tools: &[String],
        scope: &str,
    ) -> Option<Vec<String>> {
        self.build_mcp_add_http_args_full(name, url, exclude_tools, &BTreeMap::new(), scope)
    }

    pub fn build_mcp_add_http_args_full(
        &self,
        _name: &str,
        _url: &str,
        _exclude_tools: &[String],
        _headers: &BTreeMap<String, String>,
        _scope: &str,
    ) -> Option<Vec<String>> {
        None
    }

    pub fn build_mcp_remove_args(&self, _name: &str) -> Option<Vec<String>> {
        None
    }

    pub fn build_mcp_remove_args_scoped(&self, _name: &str, _scope: &str) -> Option<Vec<String>> {
        None
    }

    #[allow(dead_code)]
    pub fn build_mcp_list_args(&self) -> Option<Vec<String>> {
        None
    }

    #[allow(dead_code)]
    pub fn mcp_list_has(&self, stdout: &str, name: &str, expected_url: Option<&str>) -> MatchState {
        let has_name = stdout.lines().any(|l| l.contains(name));
        if !has_name {
            return MatchState::Missing;
        }
        match expected_url {
            Some(url) if !stdout.contains(url) => MatchState::Drift,
            _ => MatchState::MatchesName,
        }
    }

    pub fn build_filter_args(&self, filters: &McpFilters) -> Vec<String> {
        if filters.is_empty() {
            return Vec::new();
        }
        let mut args = Vec::new();
        match self {
            // Harness providers take a comma-separated, fully-qualified
            // allow/deny list (`mcp__<server>__<tool>`) that the harness
            // enforces in-registry — its own flag names, since it doesn't
            // accept claude's --allowedTools. This is the client permission
            // plane (recursion guard + brofile + per-dispatch); surface is
            // separate and server-side via the MCP URL.
            Provider::Glm | Provider::Deepseek | Provider::Brodex | Provider::VibeBh => {
                let deny = expand_filter_patterns(&filters.disallow);
                if !deny.is_empty() {
                    args.push("--deny-tools".into());
                    args.push(deny.join(","));
                }
                let allow = expand_filter_patterns(&filters.allow);
                if !allow.is_empty() {
                    args.push("--allow-tools".into());
                    args.push(allow.join(","));
                }
            }
            Provider::Workflow => {}
        }
        args
    }

    #[allow(dead_code)]
    pub fn supports_dispatch_filter(&self) -> bool {
        matches!(
            self,
            Provider::Glm
                | Provider::Deepseek
                | Provider::Brodex
                | Provider::VibeBh
        )
    }

    /// Translate a normalized fleet MCP server map into provider-native dispatch
    /// args. One config, every provider — the translation is the only
    /// per-provider concern. Today only the `--mcp-config` family (Claude and
    /// the bro-harness providers) is implemented; everything else returns empty
    /// (no per-dispatch MCP-injection seam in its CLI yet). When a future
    /// provider grows one, it slots in here against the same input map.
    pub fn build_fleet_mcp_args(
        &self,
        servers: &BTreeMap<String, mcp::McpServerConfig>,
    ) -> Vec<String> {
        if servers.is_empty() {
            return Vec::new();
        }
        match self {
            Provider::Glm
            | Provider::Deepseek
            | Provider::Brodex
            | Provider::VibeBh => {
                vec!["--mcp-config".into(), fleet_mcp_config_json(servers)]
            }
            Provider::Workflow => Vec::new(),
        }
    }
}

/// Build a Claude/harness `--mcp-config` JSON blob (`{"mcpServers":{…}}`) from a
/// normalized fleet MCP server map. `$secret` header/env refs are resolved to
/// concrete strings here — the wire form the CLIs expect. A server whose secret
/// can't resolve is skipped with a warning rather than failing the whole
/// dispatch; the rest still load.
pub fn fleet_mcp_config_json(servers: &BTreeMap<String, mcp::McpServerConfig>) -> String {
    use mcp::McpServerConfig as C;
    let mut map = serde_json::Map::new();
    for (name, cfg) in servers {
        let resolved = match cfg.resolve_secrets() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(server = %name,
                    "fleet MCP: skipping server (secret resolve failed): {e:#}");
                continue;
            }
        };
        let mut entry = serde_json::Map::new();
        match cfg {
            C::Http { url, .. } => {
                entry.insert("type".into(), "http".into());
                entry.insert("url".into(), url.clone().into());
                if !resolved.headers.is_empty() {
                    entry.insert("headers".into(), to_json_object(&resolved.headers));
                }
            }
            C::Sse { url, .. } => {
                entry.insert("type".into(), "sse".into());
                entry.insert("url".into(), url.clone().into());
                if !resolved.headers.is_empty() {
                    entry.insert("headers".into(), to_json_object(&resolved.headers));
                }
            }
            C::Stdio { command, args, .. } => {
                entry.insert("type".into(), "stdio".into());
                entry.insert("command".into(), command.clone().into());
                if !args.is_empty() {
                    entry.insert("args".into(), args.clone().into());
                }
                if !resolved.env.is_empty() {
                    entry.insert("env".into(), to_json_object(&resolved.env));
                }
            }
        }
        map.insert(name.clone(), Value::Object(entry));
    }
    serde_json::json!({ "mcpServers": Value::Object(map) }).to_string()
}

fn to_json_object(m: &BTreeMap<String, String>) -> Value {
    Value::Object(
        m.iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    )
}

#[cfg(test)]
fn emit_codex_filter_overrides(args: &mut Vec<String>, patterns: &[String], key: &str) {
    if patterns.is_empty() {
        return;
    }
    let groups = codex_group_patterns_by_server(patterns);
    if groups.is_empty() {
        tracing::warn!(target: "blackbox::filter",
            "codex {key} patterns yielded zero matches: {patterns:?}");
        return;
    }
    for (server, tools) in groups {
        let toml_array = format_toml_string_array(&tools);
        args.push("-c".into());
        args.push(format!("mcp_servers.{server}.{key}={toml_array}"));
    }
}

#[cfg(test)]
fn codex_group_patterns_by_server(patterns: &[String]) -> Vec<(String, Vec<String>)> {
    let universe: Vec<&str> = crate::tool_docs::all_tool_names();
    let mut by_server: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in patterns.iter().map(|p| mcp::normalize_filter_pattern(p)) {
        let Some(tool) = mcp::McpToolRef::parse(&p) else {
            tracing::debug!(target: "blackbox::filter",
                "codex skipping non-MCP pattern (filter scope is mcp_servers.*): {p}");
            continue;
        };
        let group = by_server.entry(tool.server.clone()).or_default();
        let names: Vec<String> = if tool.is_glob() {
            if tool.is_blackbox() {
                let expanded = mcp::expand_pattern(&tool.pattern, &universe);
                if expanded.is_empty() {
                    tracing::warn!(target: "blackbox::filter",
                        "codex blackbox glob matched zero tools (typo or stale name?): {p}");
                    continue;
                }
                expanded
            } else {
                tracing::warn!(target: "blackbox::filter",
                    "codex glob on non-blackbox server (no tool universe to expand against): {p}");
                continue;
            }
        } else {
            vec![tool.pattern.clone()]
        };
        for t in names {
            if !group.contains(&t) {
                group.push(t);
            }
        }
    }
    by_server
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

#[cfg(test)]
pub(super) fn copilot_format_mcp_tool(full: &str) -> Option<String> {
    let rest = full.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some(format!("{server}({tool})"))
}

fn expand_filter_patterns(patterns: &[String]) -> Vec<String> {
    let universe: Vec<&str> = crate::tool_docs::all_tool_names();
    let mut out = Vec::new();
    for p in patterns.iter().map(|p| mcp::normalize_filter_pattern(p)) {
        match mcp::McpToolRef::parse(&p) {
            Some(tool) if tool.is_blackbox() && tool.is_glob() => {
                for bare in mcp::expand_pattern(&tool.pattern, &universe) {
                    let full = format!("mcp__{}__{}", tool.server, bare);
                    if !out.contains(&full) {
                        out.push(full);
                    }
                }
            }
            _ => {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[cfg(test)]
pub(super) fn format_toml_string_array(items: &[String]) -> String {
    format_toml_string_array_impl(items)
}

#[cfg(test)]
fn format_toml_string_array_impl(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| toml_basic_string(s)).collect();
    format!("[{}]", quoted.join(","))
}

#[cfg(test)]
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn transient_blackbox_url() -> Option<String> {
    std::env::var("BLACKBOX_MCP_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn transient_blackbox_name() -> String {
    crate::util::blackbox_mcp_name()
}

pub fn claude_mcp_config_json(name: &str, url: &str) -> String {
    serde_json::json!({
        "mcpServers": {
            name: { "type": "http", "url": url }
        }
    })
    .to_string()
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchState {
    Missing,
    MatchesName,
    Drift,
}
