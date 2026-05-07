//! MCP server registry and filter layer.
//!
//! Users and the daemon coordinate a single view of which MCP servers
//! dispatched bros should see, and which tool calls are allowed or
//! disallowed. The registry lives under `BRO_HOME/mcp.json` (default:
//! `~/.local/state/blackbox/bro/mcp.json`) with an optional project
//! overlay at `<project>/.bro/mcp.json`.
//!
//! At dispatch time, the effective set is (global entries) merged with
//! (project entries override), and translated into provider-specific CLI
//! args — each provider has native `mcp add/list/remove` and tool-filter
//! flags (see `providers.rs::build_mcp_*_args`). Gemini and the persistent-
//! only providers are handled through their own CLI at registration time;
//! transient-injection providers (Claude, Copilot, Codex) receive the
//! effective set per-invocation as well for determinism.
//!
//! The recursion guard is now mechanical: the default filter set includes
//! disallow patterns matching the current blackbox MCP prefix's recursive
//! tool surfaces (`bro_*` orchestration plus `bbox_refactor_*` structural
//! rewrites) so dispatched agents literally cannot see (let alone call)
//! those tools unless `allow_recursion=true`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use rmcp::schemars;

use super::brofile;
use super::providers::{MatchState, Provider};

// ── Types ──────────────────────────────────────────────────────────

/// Transport-discriminated MCP server config. Matches the shape every
/// provider CLI accepts, modulo translation at registration time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
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
    /// True if this is the blackbox self-entry (used by self-registration
    /// to detect URL drift and decide add vs remove+add).
    pub fn blackbox_matches(&self, current_url: &str) -> bool {
        matches!(self, Self::Http { url, .. } if url == current_url)
    }
}

/// Filter rules — mirrors what each provider's `--disallowedTools` /
/// `--deny-tool` / `--exclude-tools` flag accepts, in a canonical form
/// translated at dispatch time.
///
/// Patterns support simple glob: `*` matches any suffix, e.g.
/// `mcp__<blackbox-name>__bro_*` matches every bro_* orchestration tool.
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

    /// Default filter set: the mechanical recursion guard. Blocks every
    /// `bro_*` orchestration tool and every `bbox_refactor_*` structural
    /// rewrite tool so dispatched agents can't spawn sub-bros or rewrite
    /// broad code surfaces unless recursion is explicitly allowed. Callers
    /// that pass allow_recursion=true skip this layer.
    pub fn default_recursion_guard() -> Self {
        Self {
            disallow: vec![
                format!("{}bro_*", crate::tool_docs::blackbox_mcp_prefix()),
                format!("{}bbox_refactor_*", crate::tool_docs::blackbox_mcp_prefix()),
            ],
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ── Path helpers ───────────────────────────────────────────────────

pub fn global_store_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| crate::util::bro_home_dir(&h).join("mcp.json"))
}

pub fn project_store_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".bro").join("mcp.json")
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
fn glob_match(pattern: &str, text: &str) -> bool {
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

// ── Self-registration ──────────────────────────────────────────────

/// Per-provider outcome for `self_register_blackbox`.
#[derive(Debug, Clone)]
pub enum SelfRegisterOutcome {
    /// Provider registered blackbox with the expected URL — nothing to do.
    Unchanged,
    /// Provider had no blackbox entry; `mcp add` succeeded.
    Added,
    /// Provider had a stale URL; removed then added.
    Updated,
    /// Provider binary isn't on PATH (spawn failed). Distinguished from
    /// ListFailed: the CLI is genuinely absent, not just misbehaving.
    NotInstalled { detail: String },
    /// Provider binary spawned but `mcp list` exited non-zero. CLI is
    /// installed but something is wrong (auth, schema mismatch, etc.).
    /// Worth surfacing differently — the user can fix this; NotInstalled
    /// requires actually installing the CLI.
    ListFailed { detail: String },
    /// Provider has no MCP CRUD (Vibe).
    Unsupported,
    /// The subsequent add/remove CLI call errored out.
    Error { detail: String },
}

#[derive(Debug, Default)]
pub struct SelfRegisterReport {
    pub per_provider: Vec<(Provider, SelfRegisterOutcome)>,
}

impl SelfRegisterReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (p, o) in &self.per_provider {
            let label = match o {
                SelfRegisterOutcome::Unchanged => "unchanged",
                SelfRegisterOutcome::Added => "added",
                SelfRegisterOutcome::Updated => "updated",
                SelfRegisterOutcome::NotInstalled { .. } => "not-installed",
                SelfRegisterOutcome::ListFailed { .. } => "list-failed",
                SelfRegisterOutcome::Unsupported => "unsupported",
                SelfRegisterOutcome::Error { .. } => "error",
            };
            parts.push(format!("{p}={label}"));
        }
        parts.join(", ")
    }
}

/// On daemon startup, ensure every provider with MCP CRUD has a
/// `name` entry pointing at `url`. Idempotent: no-op when the
/// entry is already correct; updates on drift. Per-provider failures
/// are captured and returned in the report — one missing CLI doesn't
/// block the others.
pub fn self_register_blackbox(name: &str, url: &str) -> SelfRegisterReport {
    let mut report = SelfRegisterReport::default();
    for provider in [
        Provider::Claude,
        Provider::Copilot,
        Provider::Codex,
        Provider::Gemini,
        Provider::Vibe,
    ] {
        let outcome = register_one(provider, name, url);
        report.per_provider.push((provider, outcome));
    }
    report
}

fn register_one(provider: Provider, name: &str, url: &str) -> SelfRegisterOutcome {
    let Some(list_args) = provider.build_mcp_list_args() else {
        return SelfRegisterOutcome::Unsupported;
    };

    let raw_bin = provider.bin();
    let bin = super::providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let list_out = match capture_cli_with_timeout(&provider, &list_args, None, CLI_TIMEOUT) {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // CLI installed but `mcp list` errored — distinct from
            // not-installed. User can fix this without installing.
            return SelfRegisterOutcome::ListFailed {
                detail: format!(
                    "{bin} mcp list exited {:?}: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr)
                        .lines()
                        .next()
                        .unwrap_or(""),
                ),
            };
        }
        Err(e) => {
            // Spawn failed (binary not on PATH, unreadable, etc.) OR
            // the CLI hung past CLI_TIMEOUT. Either way the daemon
            // can't talk to this provider; treat as NotInstalled so
            // self-registration doesn't block startup.
            return SelfRegisterOutcome::NotInstalled {
                detail: format!("{bin}: {e}"),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&list_out.stdout).to_string();
    // Both arg builders return None for providers without MCP CRUD
    // (today: only Vibe, which is filtered out earlier by the
    // build_mcp_list_args check). Treat unexpected None as Unsupported
    // rather than spawning the bare binary with zero args, which would
    // open an interactive REPL on most CLIs.
    let Some(add_args) = provider.build_mcp_add_http_args(name, url, &[]) else {
        return SelfRegisterOutcome::Unsupported;
    };
    match provider.mcp_list_has(&stdout, name, Some(url)) {
        MatchState::MatchesName => SelfRegisterOutcome::Unchanged,
        MatchState::Drift => {
            let Some(rm_args) = provider.build_mcp_remove_args(name) else {
                return SelfRegisterOutcome::Unsupported;
            };
            if let Err(e) = run_cli(&provider, &rm_args) {
                return SelfRegisterOutcome::Error {
                    detail: format!("remove: {e}"),
                };
            }
            match run_cli(&provider, &add_args) {
                Ok(()) => SelfRegisterOutcome::Updated,
                Err(e) => SelfRegisterOutcome::Error {
                    detail: format!("re-add: {e}"),
                },
            }
        }
        MatchState::Missing => match run_cli(&provider, &add_args) {
            Ok(()) => SelfRegisterOutcome::Added,
            Err(e) => SelfRegisterOutcome::Error {
                detail: format!("add: {e}"),
            },
        },
    }
}

/// Default timeout for provider CLI invocations. MCP CRUD calls
/// (`mcp list/add/remove`) are typically <500ms; 15s is generous
/// while still preventing one hung CLI from blocking the whole
/// fan-out loop indefinitely.
const CLI_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Provider set used by every fan-out call site (action_add /
/// action_remove / action_sync). Vibe is excluded because it has no
/// MCP CRUD CLI; the per-provider closure can short-circuit by
/// returning None when arg-builders return None.
const FANOUT_PROVIDERS: [Provider; 4] = [
    Provider::Claude,
    Provider::Copilot,
    Provider::Codex,
    Provider::Gemini,
];

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
    if let Some(env) = brofile::resolve_provider_env(*provider, None, None, Path::new("")) {
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

// ── Gemini policy file generation ──────────────────────────────────

/// Directory where per-dispatch Gemini policy files are written.
/// Daemon-owned — never touches `~/.gemini/policies/` (user territory).
pub fn gemini_policy_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| crate::util::bro_home_dir(&h).join("gemini-policies"))
}

/// Translate `McpFilters` into a Gemini-compatible policy TOML string.
///
/// Disallow patterns targeting the blackbox MCP namespace are expanded
/// against the orchestration tool universe (same treatment Codex gets).
/// Non-blackbox patterns are skipped — the Gemini policy engine keys
/// on `mcpName` + `toolName`, so filtering outside an MCP server is
/// out of scope for this translator.
pub fn render_gemini_policy_toml(filters: &McpFilters) -> String {
    let universe: Vec<&str> = crate::tool_docs::orchestration_tool_names();
    let prefix = crate::tool_docs::blackbox_mcp_prefix();
    let mcp_name = crate::util::blackbox_mcp_name();

    let mut disabled: Vec<String> = Vec::new();
    for p in filters.disallow.iter().map(|p| normalize_filter_pattern(p)) {
        let Some(stripped) = p.strip_prefix(&prefix) else {
            continue;
        };
        for t in expand_pattern(stripped, &universe) {
            if !disabled.contains(&t) {
                disabled.push(t);
            }
        }
    }

    let mut out = String::from(
        "# Generated per-dispatch by blackboxd. Do not hand-edit — this\n\
         # file is deleted when the dispatched task exits.\n",
    );

    if !disabled.is_empty() {
        let quoted: Vec<String> = disabled.iter().map(|t| format!("\"{t}\"")).collect();
        out.push_str(&format!(
            "\n[[rule]]\n\
             mcpName = \"{}\"\n\
             toolName = [{}]\n\
             decision = \"deny\"\n\
             priority = 500\n\
             denyMessage = \"Blocked by blackbox dispatch policy\"\n",
            mcp_name,
            quoted.join(",")
        ));
    }

    out
}

/// Write a Gemini policy file for a single dispatch. Returns the path
/// to the tempfile, which the caller appends to the CLI invocation via
/// `--policy <path>` and is responsible for cleaning up after the
/// child exits. Returns None if no filters apply (no file needed).
pub fn write_gemini_policy_file(task_id: &str, filters: &McpFilters) -> Result<Option<PathBuf>> {
    if filters.disallow.is_empty() && filters.allow.is_empty() {
        return Ok(None);
    }
    let dir = gemini_policy_dir().context("resolving gemini policy dir")?;
    fs::create_dir_all(&dir)?;
    let content = render_gemini_policy_toml(filters);
    // Skip the file entirely if the translator produced no rules —
    // filters targeted patterns outside blackbox namespace.
    if !content.contains("[[rule]]") {
        return Ok(None);
    }
    let path = dir.join(format!("dispatch-{task_id}.toml"));
    let mut f = fs::File::create(&path)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    Ok(Some(path))
}

/// Sweep stale Gemini policy files at daemon startup. Anything older
/// than `max_age_hours` is deleted (leftover from crashes or force-
/// kills where the normal cleanup path didn't run).
pub fn sweep_stale_gemini_policies(max_age_hours: u64) -> Result<usize> {
    let Some(dir) = gemini_policy_dir() else {
        return Ok(0);
    };
    if !dir.exists() {
        return Ok(0);
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_hours * 3600))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0;
    for entry in fs::read_dir(&dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if mtime < cutoff && path.extension().map(|e| e == "toml").unwrap_or(false) {
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
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

    let project = p
        .project
        .as_deref()
        .map(|pd| McpStore::load(&project_store_path(Path::new(pd))))
        .transpose()?;

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
    let headers = p.headers.clone().unwrap_or_default();
    let exclude = p.exclude_tools.clone().unwrap_or_default();

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
            url,
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
    let mut store = McpStore::load(&path)?;
    store.servers.insert(name.to_string(), config);
    store.save(&path)?;

    let mut lines = vec![format!("Saved {name} to {}", path.display())];
    lines.extend(fanout_lines);
    Ok(lines.join("\n"))
}

fn action_remove(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let scope = p.scope.as_deref().unwrap_or("global");

    let path = resolve_scope_path(p)?;
    let mut store = McpStore::load(&path)?;
    let had = store.servers.remove(name).is_some();
    store.save(&path)?;

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

    Ok(format!(
        "Added {} pattern {normalized} to {}",
        if disallow { "disallow" } else { "allow" },
        path.display()
    ))
}

fn action_clear_filters(p: &McpToolParams) -> Result<String> {
    let path = resolve_scope_path(p)?;
    let mut store = McpStore::load(&path)?;
    let had = !store.filters.is_empty();
    store.filters = McpFilters::default();
    store.save(&path)?;
    Ok(if had {
        format!("Cleared filters in {}", path.display())
    } else {
        format!("{} already had no filters", path.display())
    })
}

fn action_sync(p: &McpToolParams) -> Result<String> {
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;

    let mut lines = vec![format!("Syncing {} server(s)…", store.servers.len())];
    for (name, cfg) in &store.servers {
        let (url, headers) = match cfg {
            McpServerConfig::Http { url, headers, .. }
            | McpServerConfig::Sse { url, headers, .. } => (url.clone(), headers.clone()),
            McpServerConfig::Stdio { .. } => {
                lines.push(format!("  {name}: stdio not yet supported via sync"));
                continue;
            }
        };
        let exclude = cfg.exclude_tools();
        lines.extend(fanout_parallel(|provider| {
            let add_args = provider.build_mcp_add_http_args_full(name, &url, exclude, &headers, "user")?;
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
                    Some(&"Bearer token".to_string())
                );
            }
            _ => panic!("expected Http variant"),
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
                assert_eq!(headers.get("X-Auth"), Some(&"token123".to_string()));
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn self_register_outcome_distinguishes_not_installed_from_list_failed() {
        let r = SelfRegisterReport {
            per_provider: vec![
                (
                    Provider::Claude,
                    SelfRegisterOutcome::NotInstalled {
                        detail: "spawn err".into(),
                    },
                ),
                (
                    Provider::Codex,
                    SelfRegisterOutcome::ListFailed {
                        detail: "exit 1".into(),
                    },
                ),
            ],
        };
        let s = r.summary();
        assert!(s.contains("not-installed"));
        assert!(s.contains("list-failed"));
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
        assert!(eff
            .filters
            .disallow
            .contains(&"Bash(git push *)".to_string()));
        assert!(eff.filters.disallow.contains(&"Edit(*)".to_string()));
    }

    #[test]
    fn default_guard_blocks_bro_and_refactor_tools() {
        let global = McpStore::new();
        let eff = resolve_effective(&global, None, true);
        assert_eq!(
            eff.filters.disallow,
            vec![
                "mcp__blackbox__bro_*".to_string(),
                "mcp__blackbox__bbox_refactor_*".to_string()
            ]
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
    fn gemini_policy_toml_renders_deny_rule() {
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__.bro_*".to_string()],
            allow: vec![],
        };
        let toml = render_gemini_policy_toml(&filters);
        assert!(toml.contains("[[rule]]"));
        assert!(toml.contains("mcpName = \"blackbox\""));
        assert!(toml.contains("decision = \"deny\""));
        assert!(toml.contains("bro_exec"));
        assert!(toml.contains("bro_mcp"));
    }

    #[test]
    fn gemini_policy_toml_empty_when_no_blackbox_patterns() {
        let filters = McpFilters {
            disallow: vec!["Bash(rm *)".to_string()],
            allow: vec![],
        };
        let toml = render_gemini_policy_toml(&filters);
        // Only header comment, no rule block — non-blackbox patterns
        // have no Gemini policy-engine equivalent.
        assert!(!toml.contains("[[rule]]"));
    }

    #[test]
    fn write_gemini_policy_file_roundtrip() {
        // Override HOME so the write doesn't touch the user's real dir.
        let tmp = tempdir().unwrap();
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__bro_*".into()],
            allow: vec![],
        };
        let path = write_gemini_policy_file("test-task-123", &filters)
            .unwrap()
            .expect("expected a file");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("[[rule]]"));
        assert!(content.contains("bro_exec"));
        match prior {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn write_gemini_policy_file_none_when_no_filters() {
        let tmp = tempdir().unwrap();
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let filters = McpFilters::default();
        assert!(write_gemini_policy_file("t", &filters).unwrap().is_none());
        match prior {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn write_gemini_policy_file_none_when_only_non_blackbox_patterns() {
        let tmp = tempdir().unwrap();
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let filters = McpFilters {
            disallow: vec!["Edit".into()],
            allow: vec![],
        };
        // Non-blackbox patterns → no rule block → no file written.
        assert!(write_gemini_policy_file("t", &filters).unwrap().is_none());
        match prior {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
