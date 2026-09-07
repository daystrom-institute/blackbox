//! MCP server registry and filter layer.
//!
//! Users and the daemon coordinate a single view of which MCP servers
//! dispatched bros should see, and which tool calls are allowed or
//! disallowed. The registry lives under `BRO_HOME/mcp.json` (default:
//! `~/.local/state/blackbox/bro/mcp.json`) with an optional project
//! overlay at `<project>/.bbox/mcp.json`.
//!
//! At dispatch time, the effective set is (global entries) merged with
//! (project entries override) and injected into each dispatch; no
//! provider-owned MCP config file is ever rewritten. The retired
//! `bro_mcp sync` lane is kept only as an honest refusal: there is no
//! provider CLI destination to synchronize.
//!
//! The recursion guard is mechanical: the default filter set disallows
//! the current blackbox MCP prefix's dispatch-capable `bro_*`
//! orchestration tools so dispatched agents cannot spawn further
//! sub-bros unless `allow_recursion=true`. `bro_report` is excluded so
//! agents can publish progress telemetry.

use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink as make_file_symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use rmcp::schemars;

use super::providers::dispatch_prelude::*;

// ── Types ──────────────────────────────────────────────────────────

/// A string value that may be stored inline or as a reference to an
/// environment-variable secret.  The `#[serde(untagged)]` representation
/// means existing `"plain string"` JSON deserializes as `Plain("plain
/// string")` unchanged, while `{"$secret": "MY_ENV_VAR"}` deserializes as
/// `Secret { name: "MY_ENV_VAR" }`.  Writeback of Plain values emits the
/// bare string, so existing mcp.json files round-trip without modification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SecretString {
    Plain(String),
    Secret {
        #[serde(rename = "$secret")]
        name: String,
    },
}

impl SecretString {
    /// Resolve to a concrete string value.  Plain variant is identity; Secret
    /// variant reads the named environment variable, failing hard if absent.
    pub fn resolve(&self) -> anyhow::Result<String> {
        match self {
            Self::Plain(s) => Ok(s.clone()),
            Self::Secret { name } => std::env::var(name)
                .map_err(|_| anyhow::anyhow!("secret env var '{}' not set", name)),
        }
    }

    /// True if `key` (case-insensitive) matches common sensitive header names
    /// that must not be stored as inline plain text in project-local files.
    #[allow(dead_code)] // called by test-only validate_project_store
    pub fn is_sensitive_key(key: &str) -> bool {
        let lower = key.to_lowercase();
        lower.contains("authorization")
            || lower.contains("token")
            || lower.contains("secret")
            || lower.contains("api_key")
            || lower.contains("api-key")
            || lower.contains("apikey")
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self::Plain(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self::Plain(s.to_string())
    }
}

/// Resolved form of `McpServerConfig` — all `SecretString` values have been
/// looked up and are concrete strings ready to pass to provider arg builders.
pub struct ResolvedMcpServerConfig {
    pub headers: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
}

/// Transport-discriminated MCP server config. Matches the shape every
/// provider CLI accepts, modulo translation at registration time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, SecretString>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, SecretString>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, SecretString>,
    },
}

/// Endpoint origins identify the service without exposing userinfo or opaque
/// credential-bearing paths, queries, and fragments. Malformed endpoints fail
/// closed instead of echoing their raw input.
fn endpoint_origin(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.origin().ascii_serialization())
}

fn redacted_values(values: &BTreeMap<String, SecretString>) -> serde_json::Value {
    values
        .iter()
        .map(|(key, value)| {
            let view = match value {
                SecretString::Plain(_) => serde_json::json!({"redacted": true}),
                SecretString::Secret { name } => serde_json::json!({"$secret": name}),
            };
            (key.clone(), view)
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

impl McpServerConfig {
    /// Safe tool-facing configuration view. Persistence and dispatch continue to
    /// serialize the original config; no debug mode can opt out of this view.
    fn response_view(&self) -> serde_json::Value {
        match self {
            Self::Http {
                url,
                headers,
                exclude_tools,
            }
            | Self::Sse {
                url,
                headers,
                exclude_tools,
            } => {
                let transport = if matches!(self, Self::Http { .. }) {
                    "http"
                } else {
                    "sse"
                };
                serde_json::json!({
                    "type": transport,
                    "endpoint_origin": endpoint_origin(url),
                    "endpoint_redacted": true,
                    "headers": redacted_values(headers),
                    "exclude_tools": exclude_tools,
                })
            }
            Self::Stdio { command, args, env } => serde_json::json!({
                "type": "stdio",
                "command_configured": !command.is_empty(),
                "argument_count": args.len(),
                "env": redacted_values(env),
            }),
        }
    }

    /// Per-server exclude list (Gemini-only at present, applied at
    /// registration time). Empty for Stdio (no add fan-out).
    pub fn exclude_tools(&self) -> &[String] {
        match self {
            Self::Http { exclude_tools, .. } | Self::Sse { exclude_tools, .. } => exclude_tools,
            Self::Stdio { .. } => &[],
        }
    }
}

impl McpServerConfig {
    /// True if this is the blackbox self-entry at the expected URL.
    #[allow(dead_code)] // used by tests in same file
    pub fn blackbox_matches(&self, current_url: &str) -> bool {
        matches!(self, Self::Http { url, .. } if url == current_url)
    }

    /// Resolve all `SecretString` values in this config to concrete strings.
    /// Returns an error if any `$secret` reference cannot be resolved (env var
    /// not set). Provider arg builders and dispatch paths must consume the
    /// resolved form, not the raw config.
    pub fn resolve_secrets(&self) -> anyhow::Result<ResolvedMcpServerConfig> {
        let mut headers = BTreeMap::new();
        let mut env = BTreeMap::new();
        match self {
            Self::Http { headers: h, .. } | Self::Sse { headers: h, .. } => {
                for (k, v) in h {
                    headers.insert(k.clone(), v.resolve()?);
                }
            }
            Self::Stdio { env: e, .. } => {
                for (k, v) in e {
                    env.insert(k.clone(), v.resolve()?);
                }
            }
        }
        Ok(ResolvedMcpServerConfig { headers, env })
    }
}

/// Validate that a project-scoped MCP store does not contain inline plain-text
/// values for sensitive header/env keys.  Agents commit project mcp.json files;
/// secrets must travel as `{"$secret": "ENV_VAR"}` references, not bare strings.
#[allow(dead_code)] // used by tests in same file
pub fn validate_project_store(store: &McpStore) -> anyhow::Result<()> {
    for (server_name, cfg) in &store.servers {
        let pairs: Vec<(&str, &SecretString)> = match cfg {
            McpServerConfig::Http { headers, .. } | McpServerConfig::Sse { headers, .. } => {
                headers.iter().map(|(k, v)| (k.as_str(), v)).collect()
            }
            McpServerConfig::Stdio { env, .. } => {
                env.iter().map(|(k, v)| (k.as_str(), v)).collect()
            }
        };
        for (key, value) in pairs {
            if SecretString::is_sensitive_key(key) {
                if let SecretString::Plain(_) = value {
                    anyhow::bail!(
                        "project mcp.json: server '{}' key '{}' contains an inline sensitive \
                         value; use {{\"$secret\": \"ENV_VAR_NAME\"}} instead",
                        server_name,
                        key
                    );
                }
            }
        }
    }
    Ok(())
}

/// Parsed view of a filter pattern that targets an MCP server's tool
/// surface. Wire format is the canonical `mcp__<server>__<pattern>`
/// string — the type lives at parsing/recomposition boundaries only,
/// so JSON schemas, brofile files, and on-disk filters keep their
/// existing shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolRef {
    pub server: String,
    pub pattern: String,
}

impl McpToolRef {
    /// Parse a `mcp__<server>__<pattern>` string. Returns `None` for
    /// any input that isn't in MCP form — callers route those through
    /// the provider's native non-MCP path (Bash, Edit, etc.).
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("mcp__")?;
        let (server, pattern) = rest.split_once("__")?;
        if server.is_empty() || pattern.is_empty() {
            return None;
        }
        Some(Self {
            server: server.to_string(),
            pattern: pattern.to_string(),
        })
    }

    pub fn is_glob(&self) -> bool {
        self.pattern.contains('*') || self.pattern.contains('?')
    }

    pub fn is_blackbox(&self) -> bool {
        self.server == crate::util::blackbox_mcp_name()
    }
}

impl std::fmt::Display for McpToolRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mcp__{}__{}", self.server, self.pattern)
    }
}

impl std::str::FromStr for McpToolRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("not a valid mcp tool ref: {s}"))
    }
}

/// Filter rules — mirrors what each provider's `--disallowedTools` /
/// `--deny-tool` / `--exclude-tools` flag accepts, in a canonical form
/// translated at dispatch time.
///
/// Patterns support simple glob: `*` matches any suffix, e.g.
/// `mcp__<blackbox-name>__bro_*` matches every bro_* tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpFilters {
    /// Disallow rules — tools matching these patterns are filtered out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallow: Vec<String>,

    /// Allow rules — if non-empty, ONLY matching tools pass. Applied
    /// AFTER disallow (disallow always wins). Most dispatches leave this
    /// empty and rely on disallow alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl McpFilters {
    pub fn is_empty(&self) -> bool {
        self.disallow.is_empty() && self.allow.is_empty()
    }

    /// Merge another filter set into this one. `other` disallow/allow
    /// entries are appended; duplicates are deduped.
    pub fn merge_from(&mut self, other: &McpFilters) {
        for p in &other.disallow {
            let normalized = normalize_filter_pattern(p);
            if !self.disallow.iter().any(|q| q == &normalized) {
                self.disallow.push(normalized);
            }
        }
        for p in &other.allow {
            let normalized = normalize_filter_pattern(p);
            if !self.allow.iter().any(|q| q == &normalized) {
                self.allow.push(normalized);
            }
        }
    }

    /// Intersect allows from `other` into this filter set. Used by the
    /// MCP surface layer: a surface allow *narrows* the effective set,
    /// it does not widen it.
    ///
    /// Semantics:
    /// - `other.disallow` is appended additively (same as `merge_from`).
    /// - `other.allow` is intersected with `self.allow`:
    ///   - both empty → result empty (passthrough)
    ///   - self empty, other non-empty → result = expanded `other.allow`
    ///   - self non-empty, other empty → result unchanged
    ///   - both non-empty → expand both against `universe`, take set
    ///     intersection, write back as patterns. Empty intersection means
    ///     no tools pass the allow filter (everything denied).
    pub fn intersect_allow_from(&mut self, other: &McpFilters, universe: &[&str]) {
        // Disallow is always additive.
        for p in &other.disallow {
            let normalized = normalize_filter_pattern(p);
            if !self.disallow.iter().any(|q| q == &normalized) {
                self.disallow.push(normalized);
            }
        }
        // Allow intersection.
        if other.allow.is_empty() {
            return;
        }
        if self.allow.is_empty() {
            // Nothing to intersect with; adopt other's allow patterns.
            for p in &other.allow {
                let normalized = normalize_filter_pattern(p);
                if !self.allow.iter().any(|q| q == &normalized) {
                    self.allow.push(normalized);
                }
            }
            return;
        }
        // Both non-empty: expand each set, intersect, write back.
        let self_expanded: std::collections::BTreeSet<String> = self
            .allow
            .iter()
            .flat_map(|p| expand_pattern(p, universe))
            .collect();
        let other_expanded: std::collections::BTreeSet<String> = other
            .allow
            .iter()
            .flat_map(|p| expand_pattern(p, universe))
            .collect();
        let intersection: std::collections::BTreeSet<&String> =
            self_expanded.intersection(&other_expanded).collect();
        self.allow = intersection.into_iter().map(|s| (*s).clone()).collect();
    }

    /// Default filter set: the mechanical recursion guard. Blocks
    /// dispatch-capable `bro_*` orchestration tools so dispatched
    /// agents can't spawn sub-bros unless recursion is explicitly
    /// allowed. Telemetry tools like `bro_report` stay visible.
    pub fn default_recursion_guard() -> Self {
        Self {
            disallow: crate::tool_docs::recursion_guard_tool_names_prefixed(),
            allow: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpStore {
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub filters: McpFilters,
}

impl McpStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            servers: BTreeMap::new(),
            filters: McpFilters::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let raw =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::json_store::atomic_write_json_locked(path, self)
    }
}

// ── Path helpers ───────────────────────────────────────────────────

pub fn global_store_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| crate::util::bro_home_dir(&h).join("mcp.json"))
}

pub fn project_store_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".bbox").join("mcp.json")
}

#[cfg(unix)]
fn make_project_symlink(target: &Path, link: &Path) -> Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    make_file_symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn make_project_symlink(_target: &Path, _link: &Path) -> Result<()> {
    anyhow::bail!("mcp.json legacy migration is currently unsupported on this platform");
}

pub(crate) fn migrate_project_mcp_path(project_dir: &Path) -> Result<()> {
    let legacy_path = project_dir.join(".bro").join("mcp.json");
    let canonical_path = project_store_path(project_dir);

    let new_exists = canonical_path.exists();
    let old_exists = legacy_path.exists();
    if !old_exists {
        return Ok(());
    }
    if !new_exists {
        if let Some(parent) = canonical_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::rename(&legacy_path, &canonical_path).with_context(|| {
            format!(
                "moving {} to {}",
                legacy_path.display(),
                canonical_path.display()
            )
        })?;
        make_project_symlink(&canonical_path, &legacy_path)?;
        return Ok(());
    }

    let old_content = fs::read_to_string(&legacy_path)
        .with_context(|| format!("reading {}", legacy_path.display()))?;
    let new_content = fs::read_to_string(&canonical_path)
        .with_context(|| format!("reading {}", canonical_path.display()))?;
    if old_content == new_content {
        fs::remove_file(&legacy_path)
            .with_context(|| format!("replacing {}", legacy_path.display()))?;
        make_project_symlink(&canonical_path, &legacy_path)?;
    } else {
        tracing::warn!("\\.bbox/mcp.json wins; legacy .bro/mcp.json retained for review");
    }
    Ok(())
}

// ── Overlay resolution ─────────────────────────────────────────────

/// Effective view after applying project overlay on top of global.
#[derive(Debug, Clone)]
pub struct EffectiveMcp {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub filters: McpFilters,
}

/// Resolve the effective MCP set by merging global + project overlay.
/// Project entries fully replace same-named global entries. Filter
/// lists are concatenated (project additions layered on top of global).
pub fn resolve_effective(
    global: &McpStore,
    project: Option<&McpStore>,
    include_default_guard: bool,
) -> EffectiveMcp {
    let mut servers = global.servers.clone();
    let mut filters = global.filters.clone();

    if let Some(p) = project {
        for (name, cfg) in &p.servers {
            servers.insert(name.clone(), cfg.clone());
        }
        filters.merge_from(&p.filters);
    }

    if include_default_guard {
        filters.merge_from(&McpFilters::default_recursion_guard());
    }

    EffectiveMcp { servers, filters }
}

// ── Pattern matching ───────────────────────────────────────────────

/// Expand a glob-style pattern (e.g. `mcp__blackbox__bro_*`, `*_exec`,
/// `bro_?xec`) against a known tool universe. Used by providers that
/// accept exact tool names (Gemini, Codex) rather than patterns.
///
/// Supports `*` (any sequence) and `?` (single char) anywhere in the
/// pattern. Character classes (`[abc]`) are not supported — they fall
/// back to literal match against the bracketed string.
pub fn expand_pattern(pattern: &str, universe: &[&str]) -> Vec<String> {
    universe
        .iter()
        .filter(|t| glob_match(pattern, t))
        .map(|t| t.to_string())
        .collect()
}

/// Canonicalize a user/tool-facing filter pattern into the daemon's
/// internal MCP form. Accepts:
///   - canonical: `mcp__server__tool`
///   - surfaced dotted form: `mcp__server__.tool`
///   - Copilot-style MCP form: `server(tool)` (only when the inner
///     token looks like a tool name, not a shell command)
///
/// Native non-MCP patterns like `Bash(git push *)` pass through
/// unchanged.
pub fn normalize_filter_pattern(pattern: &str) -> String {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Some(rest) = trimmed.strip_prefix("mcp__") {
        if let Some((server, tool)) = rest.split_once("__.") {
            if !server.is_empty() && !tool.is_empty() {
                return format!("mcp__{server}__{tool}");
            }
        }
        if let Some((server, tool)) = rest.split_once("__") {
            if !server.is_empty() && !tool.is_empty() {
                return format!("mcp__{server}__{tool}");
            }
        }
    }

    if let Some((server, tool)) = parse_copilot_mcp_pattern(trimmed) {
        return format!("mcp__{server}__{tool}");
    }

    trimmed.to_string()
}

fn parse_copilot_mcp_pattern(pattern: &str) -> Option<(&str, &str)> {
    let (server, rest) = pattern.split_once('(')?;
    let tool = rest.strip_suffix(')')?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    if !server
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return None;
    }
    if !tool
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*' | '?' | '.' | ':'))
    {
        return None;
    }
    Some((server, tool))
}

/// Glob matcher: `*` = any sequence (incl. empty), `?` = exactly one
/// char, everything else literal. No character classes or escapes —
/// adequate for tool-name patterns we ship.
///
/// Iterative two-pointer algorithm with backtrack pointers — O(n·m)
/// worst case, no recursion. This MUST NOT regress to a recursive
/// matcher that recurses per `*` split point: that is exponential, and
/// because it runs while the per-dispatch tool-surface lock is held, a
/// pathological multi-`*` pattern wedges the WHOLE daemon (every
/// notes/thread/knowledge op stalls for minutes). Surfaced by the
/// closeout dogfooding run — see
/// design/fleet-tui/closeout-command.md §6 and thread-de03a2c5 Note 5.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // pi/ti walk pattern/text; (star, mark) remember the last '*' and
    // the text position to backtrack to when a literal run mismatches.
    let (mut pi, mut ti, mut star, mut mark): (usize, usize, Option<usize>, usize) =
        (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Mismatch under an open '*': extend the '*' by one char.
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ── MCP tool dispatch ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpAction {
    List,
    Get,
    Add,
    Remove,
    Allow,
    Disallow,
    ClearFilters,
    GetFilters,
    Sync,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpToolParams {
    pub action: McpAction,
    /// Server name (required for add/remove/get).
    #[serde(default)]
    pub name: Option<String>,
    /// URL for HTTP/SSE servers (required on add).
    #[serde(default)]
    pub url: Option<String>,
    /// Transport: http or sse (default http). stdio is rejected: no stdio
    /// add lane exists; edit the owning store directly for stdio servers.
    #[serde(default)]
    pub transport: Option<String>,
    /// Store to address: global (default) or project. list/get and every
    /// mutation read or write only the selected store. project requires
    /// the project selector; global rejects it. Unknown scopes are
    /// refused before any store access.
    #[serde(default)]
    pub scope: Option<String>,
    /// Project selector for scope=project, resolved daemon-side to the
    /// durable store key in local bridge mode. Catalog mode refuses project
    /// configuration because no remote owner transport exists. Omit for global scope.
    #[serde(default)]
    pub project: Option<String>,
    /// Filter pattern for allow/disallow (e.g. `mcp__blackbox__bro_*`).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Persistent per-server exclude list stored with the server config.
    /// Retained for compatibility; no current dispatch lane applies it.
    #[serde(default)]
    pub exclude_tools: Option<Vec<String>>,
    /// Optional HTTP/SSE headers (e.g. auth tokens) persisted into
    /// McpServerConfig and resolved only at dispatch time. Values are
    /// redacted in every reply.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// MCP tool surface name. When set on `action=add`, appends
    /// `?surface=<id>` to the registered URL.
    #[serde(default)]
    pub surface: Option<String>,
    /// action=list page size: server rows per reply (default 20, max 100; bytes may shorten the page).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue action=list after the returned next_offset.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Exact action=list/get/get_filters only: pass body.next_cursor unchanged. A
    /// changed record or selector refuses continuation; restart without
    /// cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Exact action=list/get/get_filters JSON body page byte budget; default/max
    /// 4096, min 4. On list, selects the exact redacted inventory including full server names.
    #[serde(default)]
    pub body_limit: Option<usize>,
}

/// `bro_mcp` reply shape. `Text` replies are complete and bounded at the
/// producer. `Body` replies carry an exact redacted value plus a
/// cursor-bound selection; the tool layer renders them as bounded JSON body
/// pages so a single huge accepted record cannot exceed the transport cap.
pub enum McpToolReply {
    Text(String),
    Body {
        scope: &'static str,
        selection: String,
        value: serde_json::Value,
    },
}

/// Dispatch a bro_mcp tool call. Text replies are human-readable strings;
/// Body replies are page-ready exact values (see [`McpToolReply`]).
pub fn handle(p: &McpToolParams) -> Result<McpToolReply> {
    validate_selection(p)?;
    use McpAction::*;
    match p.action {
        List if p.body_limit.is_some() || p.cursor.is_some() => action_inventory(p),
        List => action_list(p).map(McpToolReply::Text),
        Get => action_get(p),
        Add => action_add(p).map(McpToolReply::Text),
        Remove => action_remove(p).map(McpToolReply::Text),
        Allow => action_filter(p, /* disallow */ false).map(McpToolReply::Text),
        Disallow => action_filter(p, /* disallow */ true).map(McpToolReply::Text),
        ClearFilters => action_clear_filters(p).map(McpToolReply::Text),
        GetFilters => action_get_filters(p),
        Sync => action_sync(p).map(McpToolReply::Text),
    }
}

/// Closed scope vocabulary plus scope/project pairing, checked before any
/// store access so typos cannot widen into global/effective lookup and a
/// supplied project selector cannot be silently ignored.
pub(crate) fn validate_selection(p: &McpToolParams) -> Result<&'static str> {
    use McpAction::*;
    anyhow::ensure!(
        matches!(p.action, List) || (p.limit.is_none() && p.offset.is_none()),
        "limit and offset require action=list"
    );
    anyhow::ensure!(
        matches!(p.action, List | Get | GetFilters)
            || (p.cursor.is_none() && p.body_limit.is_none()),
        "cursor and body_limit require action=list, get, or get_filters"
    );
    anyhow::ensure!(
        matches!(p.action, Add)
            || (p.url.is_none()
                && p.transport.is_none()
                && p.exclude_tools.is_none()
                && p.headers.is_none()
                && p.surface.is_none()),
        "server configuration fields require action=add"
    );
    anyhow::ensure!(
        matches!(p.action, Allow | Disallow) || p.pattern.is_none(),
        "pattern requires action=allow or disallow"
    );
    anyhow::ensure!(
        matches!(p.action, Add | Remove | Get) || p.name.is_none(),
        "name requires action=add, remove, or get"
    );
    anyhow::ensure!(
        !p.project.as_deref().is_some_and(|v| v.trim().is_empty()),
        "project must not be blank"
    );
    let scope = p.scope.as_deref().unwrap_or("global");
    match scope {
        "global" => {
            if p.project.is_some() {
                anyhow::bail!(
                    "'project' applies only when scope=project; omit it for global scope"
                );
            }
            Ok("global")
        }
        "project" => {
            p.project
                .as_deref()
                .context("'project' is required when scope=project")?;
            Ok("project")
        }
        other => anyhow::bail!("Unknown scope: {other}. Use: global, project"),
    }
}

fn resolve_scope_path(p: &McpToolParams) -> Result<PathBuf> {
    match validate_selection(p)? {
        "global" => global_store_path().context("resolving home dir"),
        "project" => Ok(project_store_path(Path::new(
            p.project.as_deref().expect("validated above"),
        ))),
        _ => unreachable!("validate_selection returns a closed vocabulary"),
    }
}

fn action_inventory(p: &McpToolParams) -> Result<McpToolReply> {
    anyhow::ensure!(
        p.limit.is_none() && p.offset.is_none(),
        "exact list inventory uses cursor/body_limit; omit limit and offset"
    );
    let scope = validate_selection(p)?;
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;
    let disabled = scope == "project"
        && crate::config::load_project(Path::new(p.project.as_deref().expect("validated above")))?
            .mcp
            .enabled
            == Some(false);
    let servers: BTreeMap<_, _> = store
        .servers
        .iter()
        .map(|(name, config)| (name.clone(), config.response_view()))
        .collect();
    Ok(McpToolReply::Body {
        scope,
        selection: format!("mcp_inventory:{scope}:{}", store_identity(&path)),
        value: serde_json::json!({"servers": servers, "filters": store.filters,
            "contributes_to_dispatch": !disabled}),
    })
}

fn action_list(p: &McpToolParams) -> Result<String> {
    let scope = validate_selection(p)?;
    let eff = match scope {
        "global" => {
            let global = McpStore::load(&global_store_path().context("home dir")?)?;
            resolve_effective(&global, None, false)
        }
        "project" => {
            let pd = p.project.as_deref().expect("validated above");
            let cfg = crate::config::load_project(Path::new(pd))?;
            if cfg.mcp.enabled == Some(false) {
                return Ok(format!(
                    "Project MCP is disabled in the project config; the selected project store contributes no servers. Dispatch falls back to the global store, which is a separate scope for bro_mcp list.\n"
                ));
            }
            let project = McpStore::load(&project_store_path(Path::new(pd)))?;
            resolve_effective(&project, None, false)
        }
        _ => unreachable!("validate_selection returns a closed vocabulary"),
    };
    Ok(render_server_list(
        &eff,
        scope,
        p.project.as_deref(),
        p.offset.unwrap_or(0),
        p.limit.unwrap_or(20),
    ))
}

const FILTER_DISPLAY_LIMIT: usize = 8;

/// Bound an echoed identity string (server name, filter pattern). Exact
/// values stay recoverable through the paged detail reads (get, get_filters);
/// list rows and mutation receipts never carry an unbounded echo.
const DISPLAY_ECHO_CHARS: usize = 96;
fn bounded_echo(text: &str) -> String {
    let count = text.chars().count();
    if count <= DISPLAY_ECHO_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().take(DISPLAY_ECHO_CHARS).collect();
    format!(
        "{kept}…(+{} chars truncated; exact value via the paged detail read)",
        count - DISPLAY_ECHO_CHARS
    )
}

fn render_server_list(
    eff: &EffectiveMcp,
    scope: &str,
    _project: Option<&str>,
    offset: usize,
    limit: usize,
) -> String {
    let mut out = String::new();
    let total = eff.servers.len();
    if total == 0 {
        out.push_str(&format!("No MCP servers registered in {scope} scope.\n"));
    } else if offset >= total {
        // Past-the-end offsets are honest empty pages, never a repeat of the
        // final row.
        out.push_str(&format!(
            "MCP servers, {scope} scope: no rows at offset {offset} of {total}; the list ends at row {total}.\n"
        ));
    } else {
        let limit = limit.clamp(1, 100);
        let start = offset;
        let end = (start + limit).min(total);
        let shown = &eff.servers.iter().collect::<Vec<_>>()[start..end];
        out.push_str(&format!(
            "MCP servers, {scope} scope: rows {}-{} of {total}\n",
            start + 1,
            end,
        ));
        for (name, cfg) in shown {
            let name = bounded_echo(name);
            match cfg {
                McpServerConfig::Http { url, .. } | McpServerConfig::Sse { url, .. } => {
                    let transport = if matches!(cfg, McpServerConfig::Http { .. }) {
                        "http"
                    } else {
                        "sse"
                    };
                    let origin = bounded_echo(
                        &endpoint_origin(url).unwrap_or_else(|| "[redacted endpoint]".into()),
                    );
                    out.push_str(&format!(
                        "  {name}: {transport} {origin} (endpoint details redacted)\n"
                    ));
                }
                McpServerConfig::Stdio { args, .. } => {
                    out.push_str(&format!(
                        "  {name}: stdio ({} arguments; values redacted)\n",
                        args.len()
                    ));
                }
            }
        }
        if end < total {
            out.push_str(&format!(
                "Next page: bro_mcp(action=\"list\", scope=\"{scope}\", offset={end}); preserve the same project selector\n"
            ));
        } else if start > 0 {
            out.push_str("End of server list.\n");
        }
    }

    for (label, patterns) in [
        ("Disallow", &eff.filters.disallow),
        ("Allow", &eff.filters.allow),
    ] {
        if patterns.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{label} patterns ({}):\n", patterns.len()));
        for p in patterns.iter().take(FILTER_DISPLAY_LIMIT) {
            out.push_str(&format!("  {}\n", bounded_echo(p)));
        }
        if patterns.len() > FILTER_DISPLAY_LIMIT {
            out.push_str(&format!(
                "  ... {} more pattern(s) omitted; total {}. get_filters (same scope) pages the exact filter inventory\n",
                patterns.len() - FILTER_DISPLAY_LIMIT,
                patterns.len()
            ));
        }
    }

    out.push_str("\nExact redacted inventory, including full server identities: bro_mcp(action=\"list\", body_limit=4096) in the same scope/project; continue with body.next_cursor.\n");
    if serde_json::to_vec(&out).map_or(true, |bytes| bytes.len() > 24 * 1024)
        && limit.clamp(1, 100) > 1
    {
        return render_server_list(eff, scope, _project, offset, limit.clamp(1, 100) / 2);
    }
    out
}

/// Canonical store-file identity for content-bound cursors. The digest inside
/// a cursor never discloses the path; canonicalization keeps two distinct
/// aliases of one store from splitting identity, falling back to the raw
/// path when the store is not resolvable.
fn store_identity(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|canonical| canonical.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn action_get(p: &McpToolParams) -> Result<McpToolReply> {
    let name = p.name.as_deref().context("'name' is required")?;
    let scope = validate_selection(p)?;
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;
    match store.servers.get(name) {
        Some(cfg) => Ok(McpToolReply::Body {
            scope,
            selection: format!("mcp_config:{scope}:{}:{name}", store_identity(&path)),
            value: serde_json::json!({
                "name": name,
                "config": cfg.response_view(),
            }),
        }),
        None => Ok(McpToolReply::Text(format!(
            "{}: not registered in the {scope} MCP store. The other scope is a separate store; list it explicitly with scope.",
            bounded_echo(name),
        ))),
    }
}

/// Exact filter-inventory read for the selected store. Filter patterns are
/// identity strings, not credentials; this is the exact recovery lane the
/// bounded list display points to, and it never suggests a mutating reset.
fn action_get_filters(p: &McpToolParams) -> Result<McpToolReply> {
    let scope = validate_selection(p)?;
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;
    Ok(McpToolReply::Body {
        scope,
        selection: format!("mcp_filters:{scope}:{}", store_identity(&path)),
        value: serde_json::json!({
            "disallow": store.filters.disallow,
            "allow": store.filters.allow,
        }),
    })
}

fn action_add(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let url = p.url.as_deref().context("'url' is required")?;
    let transport = p.transport.as_deref().unwrap_or("http");
    let scope = validate_selection(p)?;
    let headers: BTreeMap<String, SecretString> = p
        .headers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, SecretString::Plain(v)))
        .collect();
    let exclude = p.exclude_tools.clone().unwrap_or_default();

    // Append ?surface= to URL when surface is specified.
    let url = if let Some(surface) = &p.surface {
        let separator = if url.contains('?') { "&" } else { "?" };
        format!("{}{}surface={}", url, separator, surface)
    } else {
        url.to_string()
    };

    let config = match transport {
        "http" => McpServerConfig::Http {
            url: url.to_string(),
            headers,
            exclude_tools: exclude,
        },
        "sse" => McpServerConfig::Sse {
            url: url.to_string(),
            headers,
            exclude_tools: exclude,
        },
        other => anyhow::bail!(
            "Transport '{other}' is not supported by bro_mcp add; supported transports are http and sse. stdio servers have no add lane: the store owner must write them directly."
        ),
    };

    let path = resolve_scope_path(p)?;
    crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;
        store.servers.insert(name.to_string(), config);
        store.save(&path)
    })?;

    Ok(format!(
        "Saved {} to the {scope} MCP store (daemon-owned; values redacted in replies). Dispatched bros receive it through per-dispatch injection; no provider CLI registration exists.",
        bounded_echo(name)
    ))
}

fn action_remove(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let scope = validate_selection(p)?;

    let path = resolve_scope_path(p)?;
    let had = crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;
        let had = store.servers.remove(name).is_some();
        store.save(&path)?;
        Ok(had)
    })?;

    Ok(if had {
        format!(
            "Removed {} from the {scope} MCP store (daemon-owned).",
            bounded_echo(name)
        )
    } else {
        format!(
            "{}: not registered in the {scope} MCP store.",
            bounded_echo(name)
        )
    })
}

fn action_filter(p: &McpToolParams, disallow: bool) -> Result<String> {
    let pattern = p.pattern.as_deref().context("'pattern' is required")?;
    let normalized = normalize_filter_pattern(pattern);
    let scope = validate_selection(p)?;
    let path = resolve_scope_path(p)?;
    crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;

        let list = if disallow {
            &mut store.filters.disallow
        } else {
            &mut store.filters.allow
        };
        if list.iter().any(|p| p == &normalized) {
            return Ok(format!(
                "{} pattern {normalized} already present",
                if disallow { "disallow" } else { "allow" }
            ));
        }
        list.push(normalized.clone());
        store.save(&path)?;
        Ok(String::new())
    })?;

    Ok(format!(
        "Added {} pattern {} to the {scope} MCP store (daemon-owned).",
        if disallow { "disallow" } else { "allow" },
        bounded_echo(&normalized),
    ))
}

fn action_clear_filters(p: &McpToolParams) -> Result<String> {
    let scope = validate_selection(p)?;
    let path = resolve_scope_path(p)?;
    let had = crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;
        let had = !store.filters.is_empty();
        store.filters = McpFilters::default();
        store.save(&path)?;
        Ok(had)
    })?;
    Ok(if had {
        format!("Cleared filters in the {scope} MCP store (daemon-owned).")
    } else {
        format!("The {scope} MCP store already had no filters.")
    })
}

fn action_sync(p: &McpToolParams) -> Result<String> {
    validate_selection(p)?;
    anyhow::bail!(
        "error.mcp_sync_retired: bro_mcp sync has no destination. No dispatch provider consumes persistent provider-CLI MCP registration; dispatched bros receive per-dispatch injection from the selected store. Nothing was synchronized: configuration and secret references were not read, resolved, or changed."
    )
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn mcp_action_mismatches_refuse_before_store_access() {
        for value in [
            serde_json::json!({"action":"remove","name":"server","cursor":"stale"}),
            serde_json::json!({"action":"clear_filters","limit":2}),
            serde_json::json!({"action":"list","headers":{"Authorization":"synthetic-secret"}}),
            serde_json::json!({"action":"get_filters","name":"ignored"}),
            serde_json::json!({"action":"list","scope":"project","project":" "}),
        ] {
            let params: McpToolParams = serde_json::from_value(value).unwrap();
            assert!(validate_selection(&params).is_err());
        }
    }

    fn text_reply(reply: McpToolReply) -> String {
        match reply {
            McpToolReply::Text(text) => text,
            McpToolReply::Body { .. } => panic!("expected a complete text reply"),
        }
    }

    #[test]
    fn mcp_configuration_responses_redact_secrets_without_changing_persistence() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cfg = McpServerConfig::Http {
            url: "https://sample-user:sample-password@example.test/private-token?token=query-secret#fragment-secret".into(),
            headers: BTreeMap::from([
                ("Authorization".into(), "Bearer inline-secret".into()),
                ("X-Custom".into(), "opaque-secret".into()),
                ("X-Reference".into(), SecretString::Secret { name: "SYNTHETIC_KEY_REFERENCE".into() }),
            ]),
            exclude_tools: vec!["admin_delete".into()],
        };
        let mut store = McpStore::new();
        store.servers.insert("remote".into(), cfg.clone());
        store.save(&project_store_path(&root)).unwrap();
        let p: McpToolParams = serde_json::from_value(serde_json::json!({
            "action": "get", "name": "remote", "scope": "project", "project": root,
        }))
        .unwrap();
        let detail = crate::tools::config::page_mcp_reply(action_get(&p).unwrap(), &p).unwrap();
        let listing = render_server_list(
            &resolve_effective(&store, None, false),
            "project",
            None,
            0,
            20,
        );
        for response in [&detail, &listing] {
            for secret in [
                "sample-user",
                "sample-password",
                "private-token",
                "query-secret",
                "fragment-secret",
                "inline-secret",
                "opaque-secret",
            ] {
                assert!(!response.contains(secret), "response disclosed {secret}");
            }
            assert!(response.contains("https://example.test"));
        }
        assert!(detail.contains("SYNTHETIC_KEY_REFERENCE"));
        assert!(detail.contains("admin_delete"));
        let view = cfg.response_view();
        assert_eq!(view["headers"]["X-Custom"]["redacted"], true);
        let loaded = McpStore::load(&project_store_path(&root)).unwrap();
        assert_eq!(loaded.servers["remote"], cfg);
    }

    #[test]
    fn mcp_configuration_responses_hide_stdio_command_arguments_and_env() {
        let cfg = McpServerConfig::Stdio {
            command: "secret-command".into(),
            args: vec!["--token=secret-argument".into()],
            env: BTreeMap::from([("CUSTOM_VALUE".into(), "secret-environment".into())]),
        };
        let view = cfg.response_view();
        let mut store = McpStore::new();
        store.servers.insert("local".into(), cfg);
        let listing = render_server_list(
            &resolve_effective(&store, None, false),
            "global",
            None,
            0,
            20,
        );
        assert_eq!(view["argument_count"], 1);
        assert_eq!(view["command_configured"], true);
        assert_eq!(view["env"]["CUSTOM_VALUE"]["redacted"], true);
        for response in [view.to_string(), listing] {
            for secret in ["secret-command", "secret-argument", "secret-environment"] {
                assert!(!response.contains(secret));
            }
        }
        assert_eq!(endpoint_origin("malformed-secret-endpoint"), None);
        assert_eq!(endpoint_origin("data:text/plain,secret-value"), None);
    }

    #[test]
    fn inventory_recovers_full_names_and_bounds_encoded_listing() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("fixture");
        fs::create_dir_all(project.join(".bbox")).unwrap();
        let mut store = McpStore::new();
        for n in 0..100 {
            let name = format!("{n:03}-{}", "\u{0001}界".repeat(150));
            store.servers.insert(
                name,
                McpServerConfig::Http {
                    url: "https://unit.test/secret-path?secret=value".into(),
                    headers: BTreeMap::from([(
                        "X-Custom".into(),
                        SecretString::Plain("fixture-secret".into()),
                    )]),
                    exclude_tools: Vec::new(),
                },
            );
        }
        store.filters.allow = (0..200)
            .map(|n| format!("{n}:{}", "\u{0001}界".repeat(150)))
            .collect();
        store.filters.disallow = store.filters.allow.clone();
        store.save(&project_store_path(&project)).unwrap();
        let mut params = list_params(Some("project"), Some(project.to_str().unwrap()), 0);
        params.limit = Some(100);
        let listing = action_list(&params).unwrap();
        assert!(serde_json::to_vec(&listing).unwrap().len() <= 24 * 1024);
        assert!(listing.contains("body_limit=4096"));
        let (text, pages) = page_exact_body(&|cursor| {
            serde_json::from_value(serde_json::json!({
                "action":"list", "scope":"project", "project":project,
                "body_limit":4096, "cursor":cursor
            }))
            .unwrap()
        });
        assert!(pages > 1);
        assert!(!text.contains("fixture-secret"));
        assert!(!text.contains("secret-path"));
        let recovered: serde_json::Value = serde_json::from_str(&text).unwrap();
        let names: Vec<_> = recovered["servers"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(names, store.servers.keys().cloned().collect::<Vec<_>>());
        assert_eq!(
            recovered["filters"]["allow"],
            serde_json::json!(store.filters.allow)
        );
    }

    #[test]
    fn roundtrip_http_server() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let mut store = McpStore::new();
        store.servers.insert(
            "blackbox".into(),
            McpServerConfig::Http {
                url: "http://127.0.0.1:7264/mcp".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        store.save(&path).unwrap();
        let loaded = McpStore::load(&path).unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert!(matches!(
            loaded.servers.get("blackbox"),
            Some(McpServerConfig::Http { url, .. }) if url == "http://127.0.0.1:7264/mcp"
        ));
    }

    #[test]
    fn blackbox_matches_detects_drift() {
        let cfg = McpServerConfig::Http {
            url: "http://127.0.0.1:7264/mcp".into(),
            headers: BTreeMap::new(),
            exclude_tools: Vec::new(),
        };
        assert!(cfg.blackbox_matches("http://127.0.0.1:7264/mcp"));
        assert!(!cfg.blackbox_matches("http://127.0.0.1:7263/mcp"));
    }

    #[test]
    fn roundtrip_persists_headers_and_exclude_tools() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let mut store = McpStore::new();
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer token".into());
        store.servers.insert(
            "blackbox".into(),
            McpServerConfig::Http {
                url: "http://127.0.0.1:7264/mcp".into(),
                headers,
                exclude_tools: vec!["bro_exec".into(), "bro_resume".into()],
            },
        );
        store.save(&path).unwrap();
        let loaded = McpStore::load(&path).unwrap();
        let cfg = loaded.servers.get("blackbox").unwrap();
        assert_eq!(
            cfg.exclude_tools(),
            &["bro_exec".to_string(), "bro_resume".to_string()]
        );
        match cfg {
            McpServerConfig::Http { headers, .. } => {
                assert_eq!(
                    headers.get("Authorization"),
                    Some(&SecretString::Plain("Bearer token".to_string()))
                );
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn project_mcp_path_uses_bbox() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let expected = Path::new(&project).join(".bbox").join("mcp.json");
        assert_eq!(project_store_path(Path::new(&project)), expected);
    }

    #[test]
    fn bro_mcp_add_surface_appends_to_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let params = McpToolParams {
            action: McpAction::Add,
            name: Some("test-surface".into()),
            url: Some("http://127.0.0.1:7264/mcp".into()),
            transport: Some("http".into()),
            scope: Some("project".into()),
            project: Some(project.clone()),
            pattern: None,
            exclude_tools: None,
            headers: None,
            surface: Some("readonly".into()),
            limit: None,
            offset: None,
            cursor: None,
            body_limit: None,
        };

        let result = text_reply(handle(&params).unwrap());
        assert!(
            result.contains("Saved test-surface"),
            "add should succeed: {result}"
        );

        let store = McpStore::load(&project_store_path(std::path::Path::new(&project))).unwrap();
        let cfg = store.servers.get("test-surface").unwrap();
        match cfg {
            McpServerConfig::Http { url, .. } => {
                assert!(
                    url.contains("?surface=readonly"),
                    "URL should contain ?surface=readonly, got: {url}"
                );
            }
            _ => panic!("expected HTTP config"),
        }
    }

    #[test]
    fn bro_mcp_add_without_surface_preserves_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let params = McpToolParams {
            action: McpAction::Add,
            name: Some("test-no-surface".into()),
            url: Some("http://127.0.0.1:7264/mcp".into()),
            transport: Some("http".into()),
            scope: Some("project".into()),
            project: Some(project.clone()),
            pattern: None,
            exclude_tools: None,
            headers: None,
            surface: None,
            limit: None,
            offset: None,
            cursor: None,
            body_limit: None,
        };

        let result = text_reply(handle(&params).unwrap());
        assert!(
            result.contains("Saved test-no-surface"),
            "add should succeed: {result}"
        );

        let store = McpStore::load(&project_store_path(std::path::Path::new(&project))).unwrap();
        let cfg = store.servers.get("test-no-surface").unwrap();
        match cfg {
            McpServerConfig::Http { url, .. } => {
                assert_eq!(url, "http://127.0.0.1:7264/mcp");
            }
            _ => panic!("expected HTTP config"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn project_mcp_migrates_bro_path_when_new_missing() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let legacy_path = Path::new(&project).join(".bro").join("mcp.json");
        let new_path = project_store_path(Path::new(&project));
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        McpStore::new().save(&legacy_path).unwrap();

        migrate_project_mcp_path(Path::new(&project)).unwrap();

        assert!(new_path.exists());
        assert!(legacy_path.exists());
        assert!(
            legacy_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn project_mcp_new_path_wins_on_conflict() {
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();
        let legacy_path = Path::new(&project).join(".bro").join("mcp.json");
        let new_path = project_store_path(Path::new(&project));
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(new_path.parent().unwrap()).unwrap();

        std::fs::write(&legacy_path, "legacy").unwrap();
        std::fs::write(&new_path, "current").unwrap();

        migrate_project_mcp_path(Path::new(&project)).unwrap();

        assert_eq!(std::fs::read_to_string(&legacy_path).unwrap(), "legacy");
        assert_eq!(std::fs::read_to_string(&new_path).unwrap(), "current");
    }

    #[test]
    fn project_mcp_disabled_reports_selected_store_without_global_leakage() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &root);
        }
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(project.join(".bbox")).unwrap();
        std::fs::write(
            project.join(".bbox").join("config.toml"),
            "[mcp]\nenabled = false\n",
        )
        .unwrap();

        let mut store = McpStore::new();
        store.servers.insert(
            "project".into(),
            McpServerConfig::Http {
                url: "http://project".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        let store_path = project_store_path(&project);
        store.save(&store_path).unwrap();

        let global = McpStore {
            version: 1,
            servers: {
                let mut servers = BTreeMap::new();
                servers.insert(
                    "global".into(),
                    McpServerConfig::Http {
                        url: "http://global".into(),
                        headers: BTreeMap::new(),
                        exclude_tools: Vec::new(),
                    },
                );
                servers
            },
            filters: McpFilters::default(),
        };
        global.save(&global_store_path().unwrap()).unwrap();

        let selected = action_list(&McpToolParams {
            action: McpAction::List,
            name: None,
            url: None,
            transport: None,
            scope: Some("project".into()),
            project: Some(project.to_string_lossy().into()),
            pattern: None,
            exclude_tools: None,
            headers: None,
            surface: None,
            limit: None,
            offset: None,
            cursor: None,
            body_limit: None,
        })
        .unwrap();

        assert!(selected.contains("Project MCP is disabled"));
        assert!(!selected.contains("global:"));
        assert!(!selected.contains("project:"));

        let global_view = action_list(&McpToolParams {
            action: McpAction::List,
            name: None,
            url: None,
            transport: None,
            scope: Some("global".into()),
            project: None,
            pattern: None,
            exclude_tools: None,
            headers: None,
            surface: None,
            limit: None,
            offset: None,
            cursor: None,
            body_limit: None,
        })
        .unwrap();
        assert!(global_view.contains("global scope"));
        assert!(global_view.contains("global:"));
        assert!(!global_view.contains("project:"));

        match prior {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    fn list_params(scope: Option<&str>, project: Option<&str>, offset: usize) -> McpToolParams {
        serde_json::from_value(serde_json::json!({
            "action": "list",
            "scope": scope,
            "project": project,
            "offset": offset,
        }))
        .unwrap()
    }

    fn http_config(url: &str) -> McpServerConfig {
        McpServerConfig::Http {
            url: url.to_string(),
            headers: BTreeMap::new(),
            exclude_tools: Vec::new(),
        }
    }

    fn with_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home) };
        let result = f();
        match prior {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result
    }

    #[test]
    fn mcp_list_validates_scope_and_selects_requested_store() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();
        let mut project_store = McpStore::new();
        project_store
            .servers
            .insert("beta".into(), http_config("http://beta.test"));
        project_store.save(&project_store_path(&project)).unwrap();

        with_home(&root, || {
            let mut global = McpStore::new();
            global
                .servers
                .insert("alpha".into(), http_config("http://alpha.test"));
            global.save(&global_store_path().unwrap()).unwrap();

            let global_view = action_list(&list_params(Some("global"), None, 0)).unwrap();
            assert!(global_view.contains("global scope"), "{global_view}");
            assert!(global_view.contains("alpha:"), "{global_view}");
            assert!(!global_view.contains("beta"), "{global_view}");

            let project_view = action_list(&list_params(
                Some("project"),
                Some(project.to_str().unwrap()),
                0,
            ))
            .unwrap();
            assert!(project_view.contains("project scope"), "{project_view}");
            assert!(project_view.contains("beta:"), "{project_view}");
            assert!(!project_view.contains("alpha"), "{project_view}");

            let unknown = action_list(&list_params(Some("typo"), None, 0)).unwrap_err();
            let text = format!("{unknown:#}");
            assert!(text.contains("Unknown scope"), "{text}");
            assert!(!text.contains("global or project"), "{text}");

            let missing = action_list(&list_params(Some("project"), None, 0)).unwrap_err();
            let text = format!("{missing:#}");
            assert!(text.contains("'project' is required"), "{text}");

            let ambiguous = action_list(&list_params(
                Some("global"),
                Some(project.to_str().unwrap()),
                0,
            ))
            .unwrap_err();
            let text = format!("{ambiguous:#}");
            assert!(text.contains("'project' applies only"), "{text}");
        });
    }

    #[test]
    fn mcp_list_pages_servers_and_bounds_filter_display() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        with_home(&root, || {
            let mut global = McpStore::new();
            for i in 0..130 {
                global
                    .servers
                    .insert(format!("srv-{i:03}"), http_config("http://unit.test"));
            }
            for i in 0..150 {
                global.filters.disallow.push(format!("pat-{i:03}"));
            }
            global.save(&global_store_path().unwrap()).unwrap();

            let first = action_list(&list_params(Some("global"), None, 0)).unwrap();
            assert!(first.contains("rows 1-20 of 130"), "{first}");
            assert!(
                first.contains("Next page: bro_mcp(action=\"list\", scope=\"global\", offset=20)"),
                "{first}"
            );
            assert!(!first.contains("srv-129"), "{first}");

            let last = action_list(&list_params(Some("global"), None, 120)).unwrap();
            assert!(last.contains("rows 121-130 of 130"), "{last}");
            assert!(last.contains("srv-129"), "{last}");
            assert!(!last.contains("Next page"), "{last}");

            assert!(last.contains("Disallow patterns (150)"), "{last}");
            assert!(last.contains("more pattern(s) omitted"), "{last}");
            assert!(
                last.contains("get_filters (same scope) pages the exact filter inventory"),
                "{last}"
            );
            assert!(!last.contains("clear_filters resets"), "{last}");
            assert!(first.len() < 8192, "first page len {}", first.len());
        });
    }

    #[test]
    fn mcp_sync_refuses_without_reading_or_resolving_secrets() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();
        let mut store = McpStore::new();
        let mut headers = BTreeMap::new();
        headers.insert(
            "X-Secret".to_string(),
            SecretString::Secret {
                name: "SYNTHETIC_MCP_TOKEN".to_string(),
            },
        );
        store.servers.insert(
            "remote".to_string(),
            McpServerConfig::Http {
                url: "http://remote.test".to_string(),
                headers,
                exclude_tools: Vec::new(),
            },
        );
        let store_file = project_store_path(&project);
        store.save(&store_file).unwrap();
        let before = fs::read_to_string(&store_file).unwrap();

        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("SYNTHETIC_MCP_TOKEN");
        unsafe { std::env::set_var("SYNTHETIC_MCP_TOKEN", "synthetic-secret-value") };

        let params: McpToolParams = serde_json::from_value(serde_json::json!({
            "action": "sync",
            "scope": "project",
            "project": project.to_str().unwrap(),
        }))
        .unwrap();
        let error = action_sync(&params).unwrap_err();
        let text = format!("{error:#}");

        assert!(text.contains("error.mcp_sync_retired"), "{text}");
        assert!(text.contains("Nothing was synchronized"), "{text}");
        assert!(
            text.contains("secret references were not read, resolved, or changed"),
            "{text}"
        );
        assert!(!text.contains("SYNTHETIC_MCP_TOKEN"), "{text}");
        assert!(!text.contains("synthetic-secret-value"), "{text}");
        assert_eq!(fs::read_to_string(&store_file).unwrap(), before);

        match prior {
            Some(value) => unsafe { std::env::set_var("SYNTHETIC_MCP_TOKEN", value) },
            None => unsafe { std::env::remove_var("SYNTHETIC_MCP_TOKEN") },
        }
    }

    #[test]
    fn mcp_add_rejects_stdio_without_provider_cli_pointer() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();

        let params: McpToolParams = serde_json::from_value(serde_json::json!({
            "action": "add",
            "name": "local",
            "transport": "stdio",
            "url": "http://ignored.test",
            "scope": "project",
            "project": project.to_str().unwrap(),
        }))
        .unwrap();
        let error = action_add(&params).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("http and sse"), "{text}");
        assert!(!text.to_lowercase().contains("provider cli"), "{text}");

        let store = McpStore::load(&project_store_path(&project)).unwrap();
        assert!(store.servers.is_empty());
    }

    #[test]
    fn mcp_mutation_replies_identify_owner_without_local_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();

        let add: McpToolParams = serde_json::from_value(serde_json::json!({
            "action": "add",
            "name": "custom-mcp",
            "transport": "http",
            "url": "http://custom.test",
            "scope": "project",
            "project": project.to_str().unwrap(),
        }))
        .unwrap();
        let reply = action_add(&add).unwrap();
        assert!(
            reply.contains("project MCP store (daemon-owned;"),
            "{reply}"
        );
        assert!(!reply.contains(".json"), "{reply}");

        let get_missing = text_reply(
            action_get(
                &serde_json::from_value::<McpToolParams>(serde_json::json!({
                    "action": "get", "name": "missing",
                    "scope": "project", "project": project.to_str().unwrap(),
                }))
                .unwrap(),
            )
            .unwrap(),
        );
        assert!(
            get_missing.contains("not registered in the project MCP store"),
            "{get_missing}"
        );
        assert!(
            !get_missing.contains("missing in project scope"),
            "{get_missing}"
        );

        let remove = action_remove(
            &serde_json::from_value::<McpToolParams>(serde_json::json!({
                "action": "remove", "name": "custom-mcp",
                "scope": "project", "project": project.to_str().unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            remove.contains("Removed custom-mcp from the project MCP store"),
            "{remove}"
        );
        assert!(!remove.contains(".json"), "{remove}");
    }

    #[test]
    fn mcp_store_parse_errors_do_not_echo_secrets() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        let store_dir = project.join(".bbox");
        fs::create_dir_all(&store_dir).unwrap();
        fs::write(
            store_dir.join("mcp.json"),
            r#"{"servers":{"x":{"type":"http","url":"https://synthetic-endpoint.example","headers":{"A":"secret-credential-value"}}},}"#,
        )
        .unwrap();

        let params = serde_json::from_value::<McpToolParams>(serde_json::json!({
            "action": "get", "name": "x",
            "scope": "project", "project": project.to_str().unwrap(),
        }))
        .unwrap();
        let error = action_get(&params).err().expect("read must reject");
        let text = format!("{error:#}");
        assert!(!text.contains("secret-credential-value"), "{text}");
        assert!(!text.contains("synthetic-endpoint"), "{text}");
    }

    #[test]
    fn mcp_filter_and_clear_replies_identify_selected_store() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();

        let filter = serde_json::from_value::<McpToolParams>(serde_json::json!({
            "action": "allow", "pattern": "mcp__blackbox__bro_exec",
            "scope": "project", "project": project.to_str().unwrap(),
        }))
        .unwrap();
        let reply = action_filter(&filter, false).unwrap();
        assert!(
            reply.contains("Added allow pattern mcp__blackbox__bro_exec to the project MCP store"),
            "{reply}"
        );

        let clear = serde_json::from_value::<McpToolParams>(serde_json::json!({
            "action": "clear_filters",
            "scope": "project", "project": project.to_str().unwrap(),
        }))
        .unwrap();
        let reply = action_clear_filters(&clear).unwrap();
        assert!(
            reply.contains("Cleared filters in the project MCP store"),
            "{reply}"
        );

        let again = action_clear_filters(&clear).unwrap();
        assert!(again.contains("already had no filters"), "{again}");
    }

    fn page_exact_body(make_params: &dyn Fn(Option<String>) -> McpToolParams) -> (String, usize) {
        let mut reconstructed = String::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            let params = make_params(cursor.clone());
            let reply =
                crate::tools::config::page_mcp_reply(handle(&params).unwrap(), &params).unwrap();
            pages += 1;
            assert!(
                reply.len() <= 4096 + 512,
                "serialized page {pages} too large: {} bytes",
                reply.len()
            );
            let page: serde_json::Value = serde_json::from_str(&reply).unwrap();
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        (reconstructed, pages)
    }

    #[test]
    fn mcp_list_offset_past_end_is_an_empty_page_not_a_repeat() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();
        let mut store = McpStore::new();
        for i in 0..3 {
            store
                .servers
                .insert(format!("srv-{i}"), http_config("http://unit.test"));
        }
        store.save(&project_store_path(&project)).unwrap();

        let empty = action_list(&list_params(
            Some("project"),
            Some(project.to_str().unwrap()),
            10,
        ))
        .unwrap();
        assert!(
            empty.contains("no rows at offset 10 of 3; the list ends at row 3"),
            "{empty}"
        );
        assert!(!empty.contains("srv-"), "{empty}");
        assert!(!empty.contains("Next page"), "{empty}");
        assert!(empty.len() < 4096, "empty page len {}", empty.len());
    }

    #[test]
    fn mcp_list_next_page_hint_carries_the_required_project_selector() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();
        let mut store = McpStore::new();
        for i in 0..25 {
            store
                .servers
                .insert(format!("srv-{i:02}"), http_config("http://unit.test"));
        }
        store.save(&project_store_path(&project)).unwrap();

        let first = action_list(&list_params(
            Some("project"),
            Some(project.to_str().unwrap()),
            0,
        ))
        .unwrap();
        assert!(first.contains("scope=\"project\", offset=20"), "{first}");
        assert!(
            first.contains("preserve the same project selector"),
            "{first}"
        );
        assert!(!first.contains("srv-24"), "{first}");
    }

    #[test]
    fn mcp_filter_inventory_recovers_exactly_via_get_filters() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project_a = root.join("project-a");
        let project_b = root.join("project-b");
        for project in [&project_a, &project_b] {
            fs::create_dir_all(project.join(".bbox")).unwrap();
        }
        let mut disallow: Vec<String> = (0..600).map(|i| format!("pat-{i:03}")).collect();
        // Escaped-string coverage: quotes, backslashes, newlines, and emoji
        // exercise JSON escaping across page boundaries.
        disallow[5] = format!("mcp__unit__{}\"quoted\\path\n🦀pattern", "x".repeat(480));
        disallow[6] = "mcp__unit__back\\slash \"tool\" *".to_string();
        let allow = vec!["mcp__unit__allow_*".to_string()];

        let mut store = McpStore::new();
        store.filters.disallow = disallow.clone();
        store.filters.allow = allow.clone();
        store.save(&project_store_path(&project_a)).unwrap();
        store.save(&project_store_path(&project_b)).unwrap();

        let listing = action_list(&list_params(
            Some("project"),
            Some(project_a.to_str().unwrap()),
            0,
        ))
        .unwrap();
        assert!(listing.contains("Disallow patterns (600)"), "{listing}");
        assert!(
            listing.contains("more pattern(s) omitted; total 600"),
            "{listing}"
        );
        assert!(
            listing.contains("get_filters (same scope) pages the exact filter inventory"),
            "{listing}"
        );
        assert!(!listing.contains("clear_filters resets"), "{listing}");
        assert!(listing.len() < 8192, "listing len {}", listing.len());

        let (reconstructed, pages) = page_exact_body(&|cursor| {
            serde_json::from_value(serde_json::json!({
                "action": "get_filters",
                "scope": "project",
                "project": project_a.to_str().unwrap(),
                "cursor": cursor,
            }))
            .unwrap()
        });
        assert!(pages > 1, "expected multiple pages, got {pages}");
        let recovered: serde_json::Value = serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(recovered["disallow"], serde_json::json!(disallow));
        assert_eq!(recovered["allow"], serde_json::json!(allow));

        // Identical filter inventories in distinct project stores must not
        // accept one another's cursors: the selection binds store identity.
        let first_page_params = serde_json::from_value::<McpToolParams>(serde_json::json!({
            "action": "get_filters",
            "scope": "project",
            "project": project_a.to_str().unwrap(),
        }))
        .unwrap();
        let first_page = crate::tools::config::page_mcp_reply(
            handle(&first_page_params).unwrap(),
            &first_page_params,
        )
        .unwrap();
        let cursor =
            serde_json::from_str::<serde_json::Value>(&first_page).unwrap()["body"]["next_cursor"]
                .as_str()
                .unwrap()
                .to_owned();
        let cross_store_params = serde_json::from_value::<McpToolParams>(serde_json::json!({
            "action": "get_filters",
            "scope": "project",
            "project": project_b.to_str().unwrap(),
            "cursor": cursor,
        }))
        .unwrap();
        assert!(
            crate::tools::config::page_mcp_reply(
                handle(&cross_store_params).unwrap(),
                &cross_store_params
            )
            .is_err()
        );
    }

    #[test]
    fn mcp_get_pages_single_huge_accepted_record_exactly() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".bbox")).unwrap();

        let huge_name = format!("huge-{}", "n".repeat(3000));
        let mut headers = BTreeMap::new();
        for i in 0..4000 {
            let value = if i % 2 == 0 {
                SecretString::Plain(format!("synthetic-secret-value-{i}"))
            } else {
                SecretString::Secret {
                    name: format!("SYNTHETIC_HEADER_KEY_{i}"),
                }
            };
            let key = if i % 100 == 7 {
                format!("weird-\"quote-{}\\path\n🦀", i)
            } else {
                format!("X-Header-{i:04}")
            };
            headers.insert(key, value);
        }
        let exclude_tools: Vec<String> = (0..2000)
            .map(|i| format!("mcp__unit__tool_{i:04}"))
            .collect();
        let http = McpServerConfig::Http {
            url: "http://unit.test/mcp".to_string(),
            headers,
            exclude_tools,
        };
        let mut env = BTreeMap::new();
        for i in 0..3000 {
            env.insert(
                format!("ENV_{i:04}"),
                if i % 2 == 0 {
                    SecretString::Plain(format!("synthetic-env-value-{i}"))
                } else {
                    SecretString::Secret {
                        name: format!("SYNTHETIC_ENV_KEY_{i}"),
                    }
                },
            );
        }
        let args: Vec<String> = (0..5000)
            .map(|i| format!("--flag-{i}=value-🦀-synthetic-argument"))
            .collect();
        let stdio = McpServerConfig::Stdio {
            command: "secret-command".to_string(),
            args,
            env,
        };
        let mut store = McpStore::new();
        store.servers.insert(huge_name.clone(), http.clone());
        store
            .servers
            .insert("stdio-huge".to_string(), stdio.clone());
        store.save(&project_store_path(&project)).unwrap();

        let listing = action_list(&list_params(
            Some("project"),
            Some(project.to_str().unwrap()),
            0,
        ))
        .unwrap();
        assert!(listing.contains("chars truncated"), "{listing}");
        assert!(listing.len() < 8192, "listing len {}", listing.len());
        assert!(!listing.contains("synthetic-secret-value"), "{listing}");

        for (name, cfg) in [(huge_name.clone(), http), ("stdio-huge".to_string(), stdio)] {
            let expected_view = cfg.response_view();
            let (reconstructed, pages) = page_exact_body(&|cursor| {
                serde_json::from_value(serde_json::json!({
                    "action": "get",
                    "name": name.clone(),
                    "scope": "project",
                    "project": project.to_str().unwrap(),
                    "cursor": cursor,
                }))
                .unwrap()
            });
            assert!(pages > 1, "{name}: expected multiple pages, got {pages}");
            assert!(
                !reconstructed.contains("synthetic-secret-value"),
                "{name}: page stream disclosed a header secret"
            );
            assert!(
                !reconstructed.contains("synthetic-env-value"),
                "{name}: page stream disclosed an env secret"
            );
            assert!(
                !reconstructed.contains("synthetic-argument"),
                "{name}: page stream disclosed a stdio argument"
            );
            assert!(
                !reconstructed.contains("secret-command"),
                "{name}: page stream disclosed the stdio command"
            );
            let recovered: serde_json::Value = serde_json::from_str(&reconstructed).unwrap();
            assert_eq!(recovered["name"], serde_json::json!(name));
            assert_eq!(recovered["config"], expected_view);
            if let Some(reference) =
                recovered["config"]["headers"]["X-Header-0001"]["$secret"].as_str()
            {
                assert_eq!(reference, "SYNTHETIC_HEADER_KEY_1");
            }
        }
    }

    #[test]
    fn exclude_tools_empty_for_stdio() {
        let cfg = McpServerConfig::Stdio {
            command: "node".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        assert!(cfg.exclude_tools().is_empty());
    }

    #[test]
    fn action_add_project_scope_persists_headers_and_exclude_tools() {
        // Project scope persists into the project store only; no provider
        // CLI fan-out exists to touch from a test.
        let dir = tempdir().unwrap();
        let project = dir.path().to_string_lossy().to_string();

        let mut headers = BTreeMap::new();
        headers.insert("X-Auth".into(), "token123".into());

        let params = McpToolParams {
            action: McpAction::Add,
            name: Some("custom-mcp".into()),
            url: Some("http://example.com/mcp".into()),
            transport: Some("http".into()),
            scope: Some("project".into()),
            project: Some(project.clone()),
            pattern: None,
            exclude_tools: Some(vec!["dangerous_tool".into(), "other_tool".into()]),
            headers: Some(headers),
            surface: None,
            limit: None,
            offset: None,
            cursor: None,
            body_limit: None,
        };
        let result = action_add(&params).unwrap();
        assert!(result.contains("Saved custom-mcp"));

        // Re-load the project store file directly and verify both
        // fields round-tripped through action_add → save.
        let store_path = project_store_path(Path::new(&project));
        let store = McpStore::load(&store_path).unwrap();
        let cfg = store.servers.get("custom-mcp").unwrap();
        assert_eq!(
            cfg.exclude_tools(),
            &["dangerous_tool".to_string(), "other_tool".to_string()]
        );
        match cfg {
            McpServerConfig::Http { url, headers, .. } => {
                assert_eq!(url, "http://example.com/mcp");
                assert_eq!(
                    headers.get("X-Auth"),
                    Some(&SecretString::Plain("token123".to_string()))
                );
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn intersect_allow_both_empty_passthrough() {
        let mut a = McpFilters::default();
        let b = McpFilters::default();
        a.intersect_allow_from(&b, &[]);
        assert!(a.allow.is_empty());
    }

    #[test]
    fn intersect_allow_self_empty_adopt_other() {
        let mut a = McpFilters::default();
        let b = McpFilters {
            allow: vec!["mcp__blackbox__bbox_search".into()],
            disallow: vec![],
        };
        let universe = &["mcp__blackbox__bbox_search", "mcp__blackbox__bbox_stats"];
        a.intersect_allow_from(&b, universe);
        assert_eq!(a.allow, vec!["mcp__blackbox__bbox_search"]);
    }

    #[test]
    fn intersect_allow_other_empty_unchanged() {
        let mut a = McpFilters {
            allow: vec!["mcp__blackbox__bbox_search".into()],
            disallow: vec![],
        };
        let b = McpFilters::default();
        a.intersect_allow_from(&b, &[]);
        assert_eq!(a.allow, vec!["mcp__blackbox__bbox_search"]);
    }

    #[test]
    fn intersect_allow_both_nonempty_takes_intersection() {
        let mut a = McpFilters {
            allow: vec![
                "mcp__blackbox__bbox_search".into(),
                "mcp__blackbox__bbox_stats".into(),
                "mcp__blackbox__bbox_forget".into(),
            ],
            disallow: vec![],
        };
        let b = McpFilters {
            allow: vec![
                "mcp__blackbox__bbox_stats".into(),
                "mcp__blackbox__bbox_forget".into(),
                "mcp__blackbox__bro_exec".into(),
            ],
            disallow: vec![],
        };
        let universe = &[
            "mcp__blackbox__bbox_search",
            "mcp__blackbox__bbox_stats",
            "mcp__blackbox__bbox_forget",
            "mcp__blackbox__bro_exec",
        ];
        a.intersect_allow_from(&b, universe);
        let mut sorted = a.allow.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["mcp__blackbox__bbox_forget", "mcp__blackbox__bbox_stats"]
        );
    }

    #[test]
    fn intersect_allow_empty_intersection_denies_all() {
        let mut a = McpFilters {
            allow: vec!["mcp__blackbox__bbox_search".into()],
            disallow: vec![],
        };
        let b = McpFilters {
            allow: vec!["mcp__blackbox__bro_exec".into()],
            disallow: vec![],
        };
        let universe = &["mcp__blackbox__bbox_search", "mcp__blackbox__bro_exec"];
        a.intersect_allow_from(&b, universe);
        assert!(a.allow.is_empty(), "empty intersection should deny all");
    }

    #[test]
    fn intersect_disallow_is_additive() {
        let mut a = McpFilters {
            allow: vec![],
            disallow: vec!["mcp__blackbox__bro_exec".into()],
        };
        let b = McpFilters {
            allow: vec![],
            disallow: vec!["mcp__blackbox__bbox_forget".into()],
        };
        a.intersect_allow_from(&b, &[]);
        assert_eq!(a.disallow.len(), 2);
        assert!(a.disallow.contains(&"mcp__blackbox__bro_exec".into()));
        assert!(a.disallow.contains(&"mcp__blackbox__bbox_forget".into()));
    }
    #[test]
    fn filters_merge_dedupes() {
        let mut a = McpFilters {
            disallow: vec!["mcp__blackbox__bro_*".into()],
            allow: vec![],
        };
        let b = McpFilters {
            disallow: vec!["mcp__blackbox__.bro_*".into(), "Bash(rm -rf *)".into()],
            allow: vec!["Read".into()],
        };
        a.merge_from(&b);
        assert_eq!(a.disallow.len(), 2);
        assert_eq!(a.allow, vec!["Read"]);
    }

    #[test]
    fn overlay_project_overrides_global() {
        let mut global = McpStore::new();
        global.servers.insert(
            "shared".into(),
            McpServerConfig::Http {
                url: "http://old/mcp".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        global.filters.disallow.push("Bash(git push *)".into());

        let mut project = McpStore::new();
        project.servers.insert(
            "shared".into(),
            McpServerConfig::Http {
                url: "http://new/mcp".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        project.filters.disallow.push("Edit(*)".into());

        let eff = resolve_effective(&global, Some(&project), false);
        assert!(matches!(
            eff.servers.get("shared"),
            Some(McpServerConfig::Http { url, .. }) if url == "http://new/mcp"
        ));
        assert_eq!(eff.filters.disallow.len(), 2);
        assert!(
            eff.filters
                .disallow
                .contains(&"Bash(git push *)".to_string())
        );
        assert!(eff.filters.disallow.contains(&"Edit(*)".to_string()));
    }

    #[test]
    fn default_guard_blocks_bro_only() {
        let global = McpStore::new();
        let eff = resolve_effective(&global, None, true);
        assert!(
            eff.filters
                .disallow
                .contains(&"mcp__blackbox__bro_exec".to_string())
        );
        assert!(
            eff.filters
                .disallow
                .contains(&"mcp__blackbox__bro_resume".to_string())
        );
        assert!(
            !eff.filters
                .disallow
                .contains(&"mcp__blackbox__bro_report".to_string())
        );
    }

    #[test]
    fn default_guard_skipped_when_disabled() {
        let global = McpStore::new();
        let eff = resolve_effective(&global, None, false);
        assert!(eff.filters.is_empty());
    }

    #[test]
    fn expand_pattern_glob_prefix() {
        let universe = [
            "mcp__blackbox__bro_exec",
            "mcp__blackbox__bro_resume",
            "mcp__blackbox__bbox_note",
            "Bash",
        ];
        let out = expand_pattern("mcp__blackbox__bro_*", &universe);
        assert_eq!(
            out,
            vec!["mcp__blackbox__bro_exec", "mcp__blackbox__bro_resume"]
        );
    }

    #[test]
    fn normalize_filter_pattern_accepts_surfaced_dotted_form() {
        assert_eq!(
            normalize_filter_pattern("mcp__blackbox__.bro_*"),
            "mcp__blackbox__bro_*"
        );
        assert_eq!(
            normalize_filter_pattern("mcp__github__.create_issue"),
            "mcp__github__create_issue"
        );
    }

    #[test]
    fn normalize_filter_pattern_accepts_copilot_mcp_form() {
        assert_eq!(
            normalize_filter_pattern("blackbox(bro_exec)"),
            "mcp__blackbox__bro_exec"
        );
        assert_eq!(
            normalize_filter_pattern("github(create_*)"),
            "mcp__github__create_*"
        );
    }

    #[test]
    fn normalize_filter_pattern_leaves_native_non_mcp_patterns_alone() {
        assert_eq!(
            normalize_filter_pattern("Bash(git push *)"),
            "Bash(git push *)"
        );
        assert_eq!(
            normalize_filter_pattern("shell(git push)"),
            "shell(git push)"
        );
    }

    #[test]
    fn expand_pattern_exact_match() {
        let universe = ["Bash", "Read", "Edit"];
        let out = expand_pattern("Bash", &universe);
        assert_eq!(out, vec!["Bash"]);
    }

    #[test]
    fn expand_pattern_supports_full_globs() {
        let universe = [
            "bro_exec",
            "bro_resume",
            "bro_status",
            "bbox_note",
            "bbox_notes",
        ];
        // Trailing `*`
        assert_eq!(expand_pattern("bro_*", &universe).len(), 3);
        // Leading `*`
        let leading = expand_pattern("*_exec", &universe);
        assert_eq!(leading, vec!["bro_exec"]);
        // Mid-string `*`
        let mid = expand_pattern("b*_note*", &universe);
        assert_eq!(mid, vec!["bbox_note", "bbox_notes"]);
        // `?` single-char wildcard
        let single = expand_pattern("bbox_note?", &universe);
        assert_eq!(single, vec!["bbox_notes"]);
        // Pure literal still works
        assert_eq!(expand_pattern("bro_exec", &universe), vec!["bro_exec"]);
        // No match returns empty (not panic)
        assert!(expand_pattern("nonexistent_*", &universe).is_empty());
    }

    #[test]
    fn mcp_plain_string_round_trips_unchanged() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let mut store = McpStore::new();
        let mut headers = BTreeMap::new();
        headers.insert(
            "X-Custom".to_string(),
            SecretString::Plain("hello".to_string()),
        );
        store.servers.insert(
            "srv".into(),
            McpServerConfig::Http {
                url: "http://host/mcp".into(),
                headers,
                exclude_tools: Vec::new(),
            },
        );
        store.save(&path).unwrap();
        // Verify the file contains the bare string (not a JSON object).
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"hello\""),
            "Plain variant must serialize as bare string"
        );
        assert!(
            !raw.contains("$secret"),
            "Plain variant must not emit $secret key"
        );
        // Reload and verify the type round-trips.
        let loaded = McpStore::load(&path).unwrap();
        match loaded.servers.get("srv").unwrap() {
            McpServerConfig::Http { headers, .. } => {
                assert_eq!(
                    headers.get("X-Custom"),
                    Some(&SecretString::Plain("hello".to_string()))
                );
            }
            _ => panic!("expected Http"),
        }
    }

    #[test]
    fn mcp_secret_reference_resolves_header() {
        let cfg = McpServerConfig::Http {
            url: "http://host/mcp".into(),
            headers: {
                let mut h = BTreeMap::new();
                h.insert(
                    "Authorization".into(),
                    SecretString::Secret {
                        name: "MY_TEST_TOKEN_12345".into(),
                    },
                );
                h
            },
            exclude_tools: Vec::new(),
        };
        // Without the env var set, resolution must fail.
        unsafe {
            std::env::remove_var("MY_TEST_TOKEN_12345");
        }
        assert!(
            cfg.resolve_secrets().is_err(),
            "missing secret must be an error"
        );

        // With it set, resolution must succeed.
        unsafe {
            std::env::set_var("MY_TEST_TOKEN_12345", "Bearer xyz123");
        }
        let resolved = cfg.resolve_secrets().unwrap();
        assert_eq!(
            resolved.headers.get("Authorization").map(|s| s.as_str()),
            Some("Bearer xyz123")
        );
        unsafe {
            std::env::remove_var("MY_TEST_TOKEN_12345");
        }
    }

    #[test]
    fn mcp_inline_sensitive_header_rejected_in_project_file() {
        let mut store = McpStore::new();
        store.servers.insert(
            "remote".into(),
            McpServerConfig::Http {
                url: "http://remote/mcp".into(),
                headers: {
                    let mut h = BTreeMap::new();
                    // Inline plain-text token in a project file should be rejected.
                    h.insert(
                        "Authorization".into(),
                        SecretString::Plain("Bearer secret".into()),
                    );
                    h
                },
                exclude_tools: Vec::new(),
            },
        );
        let result = validate_project_store(&store);
        assert!(
            result.is_err(),
            "inline sensitive header must be rejected for project stores"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("Authorization"),
            "error must name the offending key"
        );
    }

    #[test]
    fn mcp_inline_sensitive_header_allowed_in_global_file() {
        // Global stores may use inline values — developers own their own secrets.
        // Validation is only for project-scoped stores.
        let mut store = McpStore::new();
        store.servers.insert(
            "remote".into(),
            McpServerConfig::Http {
                url: "http://remote/mcp".into(),
                headers: {
                    let mut h = BTreeMap::new();
                    h.insert(
                        "Authorization".into(),
                        SecretString::Plain("Bearer secret".into()),
                    );
                    h
                },
                exclude_tools: Vec::new(),
            },
        );
        // validate_project_store is only called for project files; global files
        // don't go through this validation.  Assert the store round-trips cleanly.
        let dir = tempdir().unwrap();
        let path = dir.path().join("global-mcp.json");
        store.save(&path).unwrap();
        let loaded = McpStore::load(&path).unwrap();
        assert_eq!(loaded.servers.len(), 1);
    }

    #[test]
    fn glob_match_semantics_and_no_exponential_blowup() {
        // Core `*` / `?` / literal semantics (unchanged from the prior matcher).
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("?", "a"));
        assert!(!glob_match("?", "ab"));
        assert!(glob_match(
            "mcp__blackbox__bro_*",
            "mcp__blackbox__bro_exec"
        ));
        assert!(!glob_match(
            "mcp__blackbox__bro_*",
            "mcp__blackbox__bbox_search"
        ));
        assert!(glob_match("*bbox*", "mcp__blackbox__bbox_search"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("Bash(git push *)", "Bash(git push origin)"));

        // Regression guard: a multi-`*` pattern against a long non-matching
        // text is EXPONENTIAL under a recursive backtracking matcher and, run
        // while the dispatch tool-surface lock is held, wedges the whole daemon
        // (closeout dogfooding finding). The iterative matcher returns
        // ~instantly. If this test ever hangs or trips the time bound,
        // glob_match has regressed to recursive backtracking.
        let patho_pat = "*a*a*a*a*a*a*a*a*a*a*a*a*b";
        let patho_txt = "a".repeat(64);
        let start = std::time::Instant::now();
        assert!(!glob_match(patho_pat, &patho_txt));
        assert!(
            start.elapsed().as_millis() < 50,
            "glob_match is not linear — recursive backtracking regression"
        );
    }
}
