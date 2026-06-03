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
            Provider::Glm | Provider::Deepseek | Provider::Brodex | Provider::VibeBh
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
            Provider::Glm | Provider::Deepseek | Provider::Brodex | Provider::VibeBh => {
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
