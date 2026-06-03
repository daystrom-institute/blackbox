//! MCP server registry and filter layer.
//!
//! Users and the daemon coordinate a single view of which MCP servers
//! dispatched bros should see, and which tool calls are allowed or
//! disallowed. The registry lives under `BRO_HOME/mcp.json` (default:
//! `~/.local/state/blackbox/bro/mcp.json`) with an optional project
//! overlay at `<project>/.bbox/mcp.json`.
//!
//! At dispatch time, the effective set is (global entries) merged with
//! (project entries override), and translated into provider-specific CLI
//! args. Provider-owned MCP config files are never rewritten on daemon
//! startup; persistent provider registration happens only as the direct
//! result of explicit `bro_mcp add/remove/sync` calls.
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
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use rmcp::schemars;

use super::brofile;
use super::providers::Provider;

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

impl McpServerConfig {
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

/// Simple recursive glob matcher: `*` = any sequence (incl. empty),
/// `?` = exactly one char, everything else literal. No character
/// classes or escapes — adequate for tool-name patterns we ship.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, 0, &t, 0)
}

fn glob_match_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => (ti..=t.len()).any(|k| glob_match_inner(p, pi + 1, t, k)),
        '?' => ti < t.len() && glob_match_inner(p, pi + 1, t, ti + 1),
        c => ti < t.len() && t[ti] == c && glob_match_inner(p, pi + 1, t, ti + 1),
    }
}

/// Default timeout for provider CLI invocations. MCP CRUD calls
/// (`mcp list/add/remove`) are typically <500ms; 15s is generous
/// while still preventing one hung CLI from blocking the whole
/// fan-out loop indefinitely.
const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Provider set used by every fan-out call site (action_add /
/// action_remove / action_sync). All surviving live providers use
/// bro-harness transient per-dispatch MCP injection, so no vendor CLI gets
/// persistent MCP CRUD.
const FANOUT_PROVIDERS: [Provider; 0] = [];

/// Run a per-provider closure against FANOUT_PROVIDERS in parallel
/// using a scoped thread pool. Closures return Option<String> — None
/// drops the provider from the output (e.g. arg builder returned None),
/// Some(line) appends to the result. Order matches FANOUT_PROVIDERS.
fn fanout_parallel<F>(work: F) -> Vec<String>
where
    F: Fn(Provider) -> Option<String> + Sync,
{
    // Capture work by reference so each spawned closure can `move` the
    // reference (Copy + Send because F: Sync) instead of moving the
    // closure itself, which would only work for one spawn.
    let work = &work;
    let mut results: Vec<Option<String>> = vec![None; FANOUT_PROVIDERS.len()];
    std::thread::scope(|s| {
        let handles: Vec<_> = FANOUT_PROVIDERS
            .iter()
            .map(|&p| s.spawn(move || work(p)))
            .collect();
        for (i, h) in handles.into_iter().enumerate() {
            results[i] = h.join().unwrap_or(None);
        }
    });
    results.into_iter().flatten().collect()
}

fn run_cli(provider: &Provider, args: &[String]) -> Result<()> {
    run_cli_with_timeout(provider, args, None, CLI_TIMEOUT)
}

fn run_cli_in(provider: &Provider, args: &[String], cwd: Option<&Path>) -> Result<()> {
    run_cli_with_timeout(provider, args, cwd, CLI_TIMEOUT)
}

fn run_cli_with_timeout(
    provider: &Provider,
    args: &[String],
    cwd: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<()> {
    let out = capture_cli_with_timeout(provider, args, cwd, timeout)?;
    if !out.status.success() {
        let raw_bin = provider.bin();
        let bin = super::providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
        anyhow::bail!(
            "{bin} {} exited {:?}: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or(""),
        );
    }
    Ok(())
}

/// Spawn a provider CLI invocation with a wall-clock timeout, capturing
/// stdout + stderr. SIGKILL on timeout. `cwd` sets the child's working
/// directory — required for project-scope `mcp add` because the CLI
/// writes to <cwd>/.mcp.json (or equivalent). Returns std::process::Output
/// so callers needing the stdout (e.g. mcp list parsing) can read it.
fn capture_cli_with_timeout(
    provider: &Provider,
    args: &[String],
    cwd: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;

    let raw_bin = provider.bin();
    let bin = super::providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let mut cmd = Command::new(&bin);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(env) = brofile::resolve_provider_env(*provider, None, None, Path::new(""), None) {
        cmd.envs(env);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawning {bin}"))?;

    let start = Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(s) => break s,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("{bin} {} timed out after {:?}", args.join(" "), timeout,);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_end(&mut stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_end(&mut stderr);
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
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
    Sync,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpToolParams {
    pub action: McpAction,
    /// Server name (required for add/remove/get).
    #[serde(default)]
    pub name: Option<String>,
    /// URL for HTTP/SSE servers (required on add).
    #[serde(default)]
    pub url: Option<String>,
    /// Transport: http, sse, stdio. Defaults to http.
    #[serde(default)]
    pub transport: Option<String>,
    /// global or project (default: global).
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path — required when scope=project.
    #[serde(default)]
    pub project: Option<String>,
    /// Filter pattern for allow/disallow (e.g. `mcp__blackbox__bro_*`).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Persistent per-server exclude list (Gemini only; applied at
    /// registration time).
    #[serde(default)]
    pub exclude_tools: Option<Vec<String>>,
    /// Optional HTTP/SSE headers (e.g. auth tokens) to pass at
    /// registration time. Persisted into McpServerConfig and replayed
    /// by `action=sync`.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// MCP tool surface name. When set on `action=add`, appends
    /// `?surface=<id>` to the registered URL.
    #[serde(default)]
    pub surface: Option<String>,
}

/// Dispatch a bro_mcp tool call. Returns a human-readable result string.
pub fn handle(p: &McpToolParams) -> Result<String> {
    use McpAction::*;
    match p.action {
        List => action_list(p),
        Get => action_get(p),
        Add => action_add(p),
        Remove => action_remove(p),
        Allow => action_filter(p, /* disallow */ false),
        Disallow => action_filter(p, /* disallow */ true),
        ClearFilters => action_clear_filters(p),
        Sync => action_sync(p),
    }
}

fn resolve_scope_path(p: &McpToolParams) -> Result<PathBuf> {
    let scope = p.scope.as_deref().unwrap_or("global");
    match scope {
        "global" => global_store_path().context("resolving home dir"),
        "project" => {
            let pd = p
                .project
                .as_deref()
                .context("'project' is required when scope=project")?;
            Ok(project_store_path(Path::new(pd)))
        }
        other => anyhow::bail!("Unknown scope: {other}. Use: global, project"),
    }
}

fn action_list(p: &McpToolParams) -> Result<String> {
    let global_path = global_store_path().context("home dir")?;
    let global = McpStore::load(&global_path)?;

    let project = p.project.as_deref().map(|pd| {
        let cfg = crate::config::load_project(Path::new(pd))?;
        if cfg.mcp.enabled == Some(false) {
            return Ok(None);
        }
        McpStore::load(&project_store_path(Path::new(pd))).map(Some)
    });
    let project = project.transpose()?.flatten();

    let eff = resolve_effective(&global, project.as_ref(), false);

    let mut out = String::new();
    if eff.servers.is_empty() {
        out.push_str("No MCP servers registered.\n");
    } else {
        out.push_str(&format!("{} server(s):\n", eff.servers.len()));
        for (name, cfg) in &eff.servers {
            match cfg {
                McpServerConfig::Http { url, .. } => {
                    out.push_str(&format!("  {name} — http {url}\n"));
                }
                McpServerConfig::Sse { url, .. } => {
                    out.push_str(&format!("  {name} — sse {url}\n"));
                }
                McpServerConfig::Stdio { command, args, .. } => {
                    out.push_str(&format!("  {name} — stdio {command} {}\n", args.join(" ")));
                }
            }
        }
    }

    if !eff.filters.disallow.is_empty() {
        out.push_str(&format!("\nDisallow ({}):\n", eff.filters.disallow.len()));
        for p in &eff.filters.disallow {
            out.push_str(&format!("  {p}\n"));
        }
    }
    if !eff.filters.allow.is_empty() {
        out.push_str(&format!("\nAllow ({}):\n", eff.filters.allow.len()));
        for p in &eff.filters.allow {
            out.push_str(&format!("  {p}\n"));
        }
    }

    Ok(out)
}

fn action_get(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;
    match store.servers.get(name) {
        Some(cfg) => Ok(format!("{name}: {}", serde_json::to_string_pretty(cfg)?)),
        None => Ok(format!("{name}: not registered")),
    }
}

fn action_add(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let url = p.url.as_deref().context("'url' is required")?;
    let transport = p.transport.as_deref().unwrap_or("http");
    let scope = p.scope.as_deref().unwrap_or("global");
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
            exclude_tools: exclude.clone(),
        },
        "sse" => McpServerConfig::Sse {
            url: url.to_string(),
            headers,
            exclude_tools: exclude.clone(),
        },
        other => anyhow::bail!(
            "Transport {other} not supported via bro_mcp add (use provider CLI for stdio)"
        ),
    };

    let path = resolve_scope_path(p)?;
    let headers_for_cli = p.headers.clone().unwrap_or_default();

    // Fan out FIRST so we know whether the providers accepted the
    // add before we persist intent locally. Both global and project
    // scope fan out — project scope invokes the CLI with cwd =
    // project_dir so providers that support `-s project` write into
    // the right per-project config file.
    let cli_scope = if scope == "global" { "user" } else { "project" };
    let cwd: Option<&Path> = if scope == "project" {
        p.project.as_deref().map(Path::new)
    } else {
        None
    };
    let fanout_lines: Vec<String> = fanout_parallel(|provider| {
        let args = provider.build_mcp_add_http_args_full(
            name,
            &url,
            &exclude,
            &headers_for_cli,
            cli_scope,
        )?;
        // Idempotent: best-effort remove (no-op if absent), then add.
        // The remove error is logged but not surfaced — it's expected
        // to fail when the server isn't already registered. Genuine
        // failures (CLI crash, permissions) still surface via the
        // subsequent add error.
        if let Some(rm) = provider.build_mcp_remove_args_scoped(name, cli_scope) {
            if let Err(e) = run_cli_in(&provider, &rm, cwd) {
                tracing::debug!(target: "blackbox::mcp",
                    "{provider} idempotent pre-add remove of {name} ({cli_scope}) failed (ok if not registered): {e}");
            }
        }
        Some(match run_cli_in(&provider, &args, cwd) {
            Ok(()) => format!("  {provider} ({cli_scope}): added"),
            Err(e) => format!("  {provider} ({cli_scope}): error — {e}"),
        })
    });

    // Persist intent regardless of fan-out outcome — `sync` can replay
    // failed providers later, but only if we recorded the config.
    crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;
        store.servers.insert(name.to_string(), config);
        store.save(&path)
    })?;

    let mut lines = vec![format!("Saved {name} to {}", path.display())];
    lines.extend(fanout_lines);
    Ok(lines.join("\n"))
}

fn action_remove(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let scope = p.scope.as_deref().unwrap_or("global");

    let path = resolve_scope_path(p)?;
    let had = crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;
        let had = store.servers.remove(name).is_some();
        store.save(&path)?;
        Ok(had)
    })?;

    let mut lines = vec![if had {
        format!("Removed {name} from {}", path.display())
    } else {
        format!("{name} not in {}", path.display())
    }];

    let cli_scope = if scope == "global" { "user" } else { "project" };
    let cwd: Option<&Path> = if scope == "project" {
        p.project.as_deref().map(Path::new)
    } else {
        None
    };
    lines.extend(fanout_parallel(|provider| {
        let args = provider.build_mcp_remove_args_scoped(name, cli_scope)?;
        Some(match run_cli_in(&provider, &args, cwd) {
            Ok(()) => format!("  {provider} ({cli_scope}): removed"),
            Err(e) => format!("  {provider} ({cli_scope}): {e}"),
        })
    }));

    Ok(lines.join("\n"))
}

fn action_filter(p: &McpToolParams, disallow: bool) -> Result<String> {
    let pattern = p.pattern.as_deref().context("'pattern' is required")?;
    let normalized = normalize_filter_pattern(pattern);
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
        "Added {} pattern {normalized} to {}",
        if disallow { "disallow" } else { "allow" },
        path.display()
    ))
}

fn action_clear_filters(p: &McpToolParams) -> Result<String> {
    let path = resolve_scope_path(p)?;
    let (had, path) = crate::json_store::with_store_lock(&path.clone(), || {
        let mut store = McpStore::load(&path)?;
        let had = !store.filters.is_empty();
        store.filters = McpFilters::default();
        store.save(&path)?;
        Ok((had, path))
    })?;
    Ok(if had {
        format!("Cleared filters in {}", path.display())
    } else {
        format!("{} already had no filters", path.display())
    })
}

fn action_sync(p: &McpToolParams) -> Result<String> {
    let path = resolve_scope_path(p)?;
    let store = crate::json_store::with_store_lock(&path.clone(), || McpStore::load(&path))?;

    let mut lines = vec![format!("Syncing {} server(s)…", store.servers.len())];
    for (name, cfg) in &store.servers {
        let url = match cfg {
            McpServerConfig::Http { url, .. } | McpServerConfig::Sse { url, .. } => url.clone(),
            McpServerConfig::Stdio { .. } => {
                lines.push(format!("  {name}: stdio not yet supported via sync"));
                continue;
            }
        };
        let resolved_headers = match cfg.resolve_secrets() {
            Ok(r) => r.headers,
            Err(e) => {
                lines.push(format!("  {name}: secret resolution failed: {e}"));
                continue;
            }
        };
        let exclude = cfg.exclude_tools();
        lines.extend(fanout_parallel(|provider| {
            let add_args = provider.build_mcp_add_http_args_full(name, &url, exclude, &resolved_headers, "user")?;
            if let Some(rm) = provider.build_mcp_remove_args(name) {
                if let Err(e) = run_cli(&provider, &rm) {
                    tracing::debug!(target: "blackbox::mcp",
                        "{provider} idempotent pre-sync remove of {name} failed (ok if not registered): {e}");
                }
            }
            Some(match run_cli(&provider, &add_args) {
                Ok(()) => format!("  {name} → {provider}: synced"),
                Err(e) => format!("  {name} → {provider}: {e}"),
            })
        }));
    }
    Ok(lines.join("\n"))
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        };

        let result = handle(&params).unwrap();
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
        };

        let result = handle(&params).unwrap();
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
    fn project_mcp_disabled_ignores_overlay() {
        let dir = tempdir().unwrap();
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let project = dir.path().join("project");
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

        let result = action_list(&McpToolParams {
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
        })
        .unwrap();

        assert!(result.contains("1 server(s):"));
        assert!(result.contains("global"));

        match prior {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
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
        // Project-scope add skips provider fan-out (overlay only),
        // so we can exercise the persistence path end-to-end without
        // touching real provider CLIs.
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
}
