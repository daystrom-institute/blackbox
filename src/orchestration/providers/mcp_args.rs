use std::collections::BTreeMap;

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
        name: &str,
        url: &str,
        exclude_tools: &[String],
        headers: &BTreeMap<String, String>,
        scope: &str,
    ) -> Option<Vec<String>> {
        match self {
            Provider::Claude => {
                let scope_flag = match scope {
                    "user" | "project" | "local" => scope,
                    _ => return None,
                };
                let mut args = vec![
                    "mcp".into(),
                    "add".into(),
                    "-s".into(),
                    scope_flag.into(),
                    "--transport".into(),
                    "http".into(),
                ];
                for (k, v) in headers {
                    args.push("-H".into());
                    args.push(format!("{k}: {v}"));
                }
                args.extend([name.into(), url.into()]);
                Some(args)
            }
            Provider::Inception => None,
            Provider::Copilot => {
                if scope != "user" {
                    return None;
                }
                if !headers.is_empty() {
                    tracing::debug!(target: "blackbox::mcp",
                        "copilot mcp add: dropping {} header(s) (no documented header flag)",
                        headers.len());
                }
                Some(vec![
                    "copilot".into(),
                    "--".into(),
                    "mcp".into(),
                    "add".into(),
                    "--transport".into(),
                    "http".into(),
                    name.into(),
                    url.into(),
                ])
            }
            Provider::Codex => {
                if scope != "user" {
                    return None;
                }
                if !headers.is_empty() {
                    tracing::debug!(target: "blackbox::mcp",
                        "codex mcp add: dropping {} header(s) (only --bearer-token-env-var supported)",
                        headers.len());
                }
                Some(vec![
                    "mcp".into(),
                    "add".into(),
                    name.into(),
                    "--url".into(),
                    url.into(),
                ])
            }
            Provider::Gemini => {
                let scope_flag = match scope {
                    "user" | "project" => scope,
                    _ => return None,
                };
                let mut args = vec![
                    "mcp".into(),
                    "add".into(),
                    "-t".into(),
                    "http".into(),
                    "-s".into(),
                    scope_flag.into(),
                ];
                if !exclude_tools.is_empty() {
                    args.extend(["--exclude-tools".into(), exclude_tools.join(",")]);
                }
                for (k, v) in headers {
                    args.push("-H".into());
                    args.push(format!("{k}: {v}"));
                }
                args.extend([name.into(), url.into()]);
                Some(args)
            }
            // Harness providers (glm/deepseek/brodex) use transient inline
            // --mcp-config injection, not vendor-CLI mcp add/remove/list.
            Provider::Glm | Provider::Deepseek | Provider::Brodex => None,
            Provider::Vibe | Provider::Workflow => None,
        }
    }

    pub fn build_mcp_remove_args(&self, name: &str) -> Option<Vec<String>> {
        self.build_mcp_remove_args_scoped(name, "user")
    }

    pub fn build_mcp_remove_args_scoped(&self, name: &str, scope: &str) -> Option<Vec<String>> {
        match self {
            Provider::Claude => {
                let scope_flag = match scope {
                    "user" | "project" | "local" => scope,
                    _ => return None,
                };
                Some(vec![
                    "mcp".into(),
                    "remove".into(),
                    "-s".into(),
                    scope_flag.into(),
                    name.into(),
                ])
            }
            Provider::Inception => None,
            Provider::Copilot => {
                if scope != "user" {
                    return None;
                }
                Some(vec![
                    "copilot".into(),
                    "--".into(),
                    "mcp".into(),
                    "remove".into(),
                    name.into(),
                ])
            }
            Provider::Codex => {
                if scope != "user" {
                    return None;
                }
                Some(vec!["mcp".into(), "remove".into(), name.into()])
            }
            Provider::Gemini => {
                let scope_flag = match scope {
                    "user" | "project" => scope,
                    _ => return None,
                };
                Some(vec![
                    "mcp".into(),
                    "remove".into(),
                    "-s".into(),
                    scope_flag.into(),
                    name.into(),
                ])
            }
            // Harness providers (glm/deepseek/brodex) use transient inline
            // --mcp-config injection, not vendor-CLI mcp add/remove/list.
            Provider::Glm | Provider::Deepseek | Provider::Brodex => None,
            Provider::Vibe | Provider::Workflow => None,
        }
    }

    #[allow(dead_code)]
    pub fn build_mcp_list_args(&self) -> Option<Vec<String>> {
        match self {
            Provider::Claude => Some(vec!["mcp".into(), "list".into()]),
            Provider::Inception => None,
            Provider::Copilot => Some(vec![
                "copilot".into(),
                "--".into(),
                "mcp".into(),
                "list".into(),
            ]),
            Provider::Codex => Some(vec!["mcp".into(), "list".into()]),
            Provider::Gemini => Some(vec!["mcp".into(), "list".into()]),
            // Harness providers (glm/deepseek/brodex) use transient inline
            // --mcp-config injection, not vendor-CLI mcp add/remove/list.
            Provider::Glm | Provider::Deepseek | Provider::Brodex => None,
            Provider::Vibe | Provider::Workflow => None,
        }
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
            Provider::Claude => {
                let expanded = expand_filter_patterns(&filters.disallow);
                if !expanded.is_empty() {
                    args.push("--disallowedTools".into());
                    args.push(expanded.join(" "));
                }
                let expanded_allow = expand_filter_patterns(&filters.allow);
                if !expanded_allow.is_empty() {
                    args.push("--allowedTools".into());
                    args.push(expanded_allow.join(" "));
                }
            }
            // Harness providers take a comma-separated, fully-qualified
            // allow/deny list (`mcp__<server>__<tool>`) that the harness
            // enforces in-registry — its own flag names, since it doesn't
            // accept claude's --allowedTools. This is the client permission
            // plane (recursion guard + brofile + per-dispatch); surface is
            // separate and server-side via the MCP URL.
            Provider::Glm | Provider::Deepseek | Provider::Brodex => {
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
            Provider::Inception => {}
            Provider::Copilot => {
                for p in expand_filter_patterns(&filters.disallow) {
                    args.push(format!(
                        "--deny-tool={}",
                        copilot_format_mcp_tool(&p).unwrap_or(p)
                    ));
                }
                for p in expand_filter_patterns(&filters.allow) {
                    args.push(format!(
                        "--allow-tool={}",
                        copilot_format_mcp_tool(&p).unwrap_or(p)
                    ));
                }
            }
            Provider::Codex => {
                emit_codex_filter_overrides(&mut args, &filters.disallow, "disabled_tools");
                emit_codex_filter_overrides(&mut args, &filters.allow, "enabled_tools");
            }
            Provider::Gemini => {}
            Provider::Vibe => {
                for p in expand_filter_patterns(&filters.allow) {
                    args.push("--enabled-tools".into());
                    args.push(p);
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
            Provider::Claude
                | Provider::Copilot
                | Provider::Codex
                | Provider::Gemini
                | Provider::Vibe
        )
    }
}

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

#[cfg(not(test))]
fn format_toml_string_array(items: &[String]) -> String {
    format_toml_string_array_impl(items)
}

fn format_toml_string_array_impl(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| toml_basic_string(s)).collect();
    format!("[{}]", quoted.join(","))
}

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
