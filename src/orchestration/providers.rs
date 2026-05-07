use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Provider {
    Claude,
    #[serde(alias = "opencode")]
    #[strum(serialize = "glm", serialize = "opencode")]
    Glm,
    Deepseek,
    Inception,
    Codex,
    Copilot,
    Vibe,
    Gemini,
    Workflow,
}

/// Capability tag advertised by providers and required by workflow
/// nodes. Workflow compile validates that every actor's resolved
/// provider stack covers the node's `requires` set; missing
/// capabilities are a hard compile error, not a silent downgrade.
///
/// Mirrors daystrom's `CapabilityTag` shape (see
/// `daystrom-mk2/src/Daystrom.AgentSdk/Providers/IAgentProvider.cs`)
/// — the lesson there was that silent fallback when a provider
/// can't honor a feature flag (Gemini's structured output story)
/// caused multi-hour debugging sessions. Hard error at compile time
/// is the right call.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Capability {
    /// Native JSON-schema-backed structured output. Codex
    /// (--output-schema), Claude (extension-passed schema). Gemini
    /// CLI does NOT (per daystrom GeminiProvider.cs:49).
    StructuredOutput,
    /// Image input (vision).
    Vision,
    /// Long context (≥1M tokens). Claude Opus 4.7 (built-in 1M),
    /// Claude Opus 4.6[1m] (1M variant). Smaller models excluded.
    LongContext,
    /// Native tool/function call dispatch (vs. text-only output).
    ToolUse,
    /// Session resume (`--resume <id>`). Used by every actor with
    /// `durable: true`.
    Resume,
}

impl Provider {
    pub const ALL: &[Provider] = &[
        Provider::Claude,
        Provider::Glm,
        Provider::Deepseek,
        Provider::Inception,
        Provider::Codex,
        Provider::Copilot,
        Provider::Vibe,
        Provider::Gemini,
    ];

    /// Capabilities this provider offers regardless of model. Per-model
    /// overrides are NOT modeled today (matches daystrom's per-provider
    /// flag) — when that becomes a real distinction (e.g. only
    /// Opus 4.7 has true 1M context vs. 4.6's [1m] variant), promote
    /// `LongContext` to be model-keyed and update this method's signature
    /// to take a model id.
    pub fn capabilities(&self) -> std::collections::HashSet<Capability> {
        use Capability::*;
        let v: &[Capability] = match self {
            Provider::Claude => &[StructuredOutput, Vision, LongContext, ToolUse, Resume],
            Provider::Codex => &[StructuredOutput, ToolUse, Resume],
            Provider::Glm => &[ToolUse, Resume],
            Provider::Deepseek => &[ToolUse, Resume],
            Provider::Inception => &[ToolUse, Resume],
            Provider::Copilot => &[ToolUse, Resume],
            Provider::Gemini => &[Vision, ToolUse],
            Provider::Vibe => &[],
            Provider::Workflow => &[],
        };
        v.iter().copied().collect()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Glm => "glm",
            Provider::Deepseek => "deepseek",
            Provider::Inception => "inception",
            Provider::Codex => "codex",
            Provider::Copilot => "copilot",
            Provider::Vibe => "vibe",
            Provider::Gemini => "gemini",
            Provider::Workflow => "workflow",
        }
    }

    pub fn bin(&self) -> String {
        match self {
            Provider::Claude => std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
            Provider::Glm => std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".into()),
            Provider::Deepseek => {
                std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".into())
            }
            Provider::Inception => {
                std::env::var("OPENCODE_BIN").unwrap_or_else(|_| "opencode".into())
            }
            Provider::Codex => std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into()),
            Provider::Copilot => std::env::var("COPILOT_BIN").unwrap_or_else(|_| "gh".into()),
            Provider::Vibe => std::env::var("VIBE_BIN").unwrap_or_else(|_| "vibe".into()),
            Provider::Gemini => std::env::var("GEMINI_BIN").unwrap_or_else(|_| "gemini".into()),
            Provider::Workflow => "workflow".into(),
        }
    }

    pub fn supports_resume(&self) -> bool {
        matches!(
            self,
            Provider::Claude
                | Provider::Glm
                | Provider::Deepseek
                | Provider::Inception
                | Provider::Codex
                | Provider::Copilot
                | Provider::Vibe
                | Provider::Gemini
        )
    }

    pub fn is_streaming_json(&self) -> bool {
        matches!(
            self,
            Provider::Claude
                | Provider::Glm
                | Provider::Deepseek
                | Provider::Inception
                | Provider::Codex
                | Provider::Copilot
        )
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        match self {
            Provider::Claude => CLAUDE_MODELS,
            Provider::Glm => GLM_MODELS,
            Provider::Deepseek => DEEPSEEK_MODELS,
            Provider::Inception => INCEPTION_MODELS,
            Provider::Codex => CODEX_MODELS,
            Provider::Copilot => COPILOT_MODELS,
            Provider::Vibe => VIBE_MODELS,
            Provider::Gemini => GEMINI_MODELS,
            Provider::Workflow => &[],
        }
    }

    pub fn efforts(&self) -> &'static [EffortInfo] {
        match self {
            Provider::Claude => CLAUDE_EFFORTS,
            Provider::Glm => OPENCODE_VARIANTS,
            Provider::Deepseek => OPENCODE_VARIANTS,
            Provider::Inception => OPENCODE_VARIANTS,
            Provider::Codex => CODEX_EFFORTS,
            Provider::Copilot => COPILOT_EFFORTS,
            _ => &[],
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Binary resolution
// ---------------------------------------------------------------------------

/// Resolve a provider binary name to an absolute path using a login shell.
///
/// The daemon is typically launched from `launchctl` / `systemd` with a
/// narrow, static `PATH` — it does not source `.bashrc`, `.zshrc`, `nvm.sh`,
/// or other rc files. CLIs installed under a version manager (nvm, asdf,
/// rbenv, etc.) live in per-version directories that only get added to
/// PATH by shell rc init. Running `bash -lc "command -v <bin>"` invokes a
/// login shell so those additions fire, giving us the same resolution a
/// user would get in an interactive terminal.
///
/// If `bin` already contains a path separator it is returned as-is, which
/// preserves explicit `CODEX_BIN=/custom/path/codex` overrides.
///
/// Returns `None` if the binary cannot be resolved. Callers should fall
/// back to the bare name so `Command::new` produces the familiar
/// `No such file or directory` error at spawn time instead of a silent
/// nothing.
pub fn resolve_bin(bin: &str) -> Option<String> {
    if bin.contains('/') {
        return Some(bin.to_string());
    }
    let extra_path = std::env::var("BRO_EXTRA_PATH").unwrap_or_else(|_| {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".local/bin")
            .to_string_lossy()
            .to_string()
    });
    let augmented_path = format!(
        "{}:{}",
        extra_path,
        std::env::var("PATH").unwrap_or_default()
    );
    let output = std::process::Command::new("bash")
        .args(["-lc", &format!("command -v '{bin}'")])
        .env("PATH", &augmented_path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

// ---------------------------------------------------------------------------
// Exec options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub model: Option<String>,
    pub effort: Option<String>,
}

// ---------------------------------------------------------------------------
// Arg builders
// ---------------------------------------------------------------------------

impl Provider {
    pub fn build_exec_args(
        &self,
        prompt: &str,
        session_id: &str,
        cwd: Option<&str>,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts.and_then(|o| o.model.as_deref());
        let effort = opts.and_then(|o| o.effort.as_deref());

        match self {
            Provider::Claude => {
                let mut args = vec![
                    "-p".into(),
                    prompt.into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--include-partial-messages".into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if !session_id.is_empty() && session_id != "pending" {
                    args.extend(["--session-id".into(), session_id.into()]);
                }
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                // Transient MCP inject — ensures dispatched subprocesses
                // see blackbox regardless of which config file the bare
                // `claude` CLI happens to load ($HOME/.claude.json vs
                // account-specific). Augments whatever user config the
                // subprocess would otherwise inherit.
                if let Some(url) = transient_blackbox_url() {
                    let name = transient_blackbox_name();
                    args.extend(["--mcp-config".into(), claude_mcp_config_json(&name, &url)]);
                }
                args
            }
            Provider::Glm | Provider::Deepseek | Provider::Inception => {
                let mut args = vec![
                    "run".into(),
                    "--format".into(),
                    "json".into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--variant".into(), e.into()]);
                }
                if let Some(c) = cwd {
                    args.extend(["--dir".into(), c.into()]);
                }
                args.push(prompt.into());
                args
            }
            Provider::Codex => {
                let mut args = vec![
                    "exec".into(),
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                    "--json".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
                }
                if let Some(c) = cwd {
                    args.extend(["-C".into(), c.into()]);
                }
                args.push(prompt.into());
                args
            }
            Provider::Copilot => {
                let mut args = vec![
                    "copilot".into(),
                    "--".into(),
                    "-p".into(),
                    prompt.into(),
                    "--yolo".into(),
                    "--autopilot".into(),
                    "--output-format".into(),
                    "json".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                if let Some(c) = cwd {
                    args.extend(["--add-dir".into(), c.into()]);
                }
                args
            }
            Provider::Vibe => {
                // Vibe CLI has no `--model` flag — model is selected
                // out-of-band via `--agent NAME` (~/.vibe/agents/*.toml)
                // or `vibe --setup`. Ignore opts.model.
                let _ = model;
                vec!["-p".into(), prompt.into(), "--output".into(), "json".into()]
            }
            Provider::Gemini => {
                let mut args = vec![
                    "-p".into(),
                    prompt.into(),
                    "--yolo".into(),
                    "-o".into(),
                    "json".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                args
            }
            Provider::Workflow => Vec::new(),
        }
    }

    /// Locate the cwd a prior session was recorded in. Enables agents
    /// to resume across repo boundaries without hand-passing project_dir.
    /// Returns None when the provider has no cwd-aware session store or
    /// when the session can't be found locally.
    pub fn resolve_session_cwd(&self, session_id: &str) -> Option<std::path::PathBuf> {
        match self {
            Provider::Claude => resolve_claude_session_cwd(session_id),
            Provider::Glm | Provider::Deepseek | Provider::Inception => None,
            Provider::Codex => resolve_codex_session_cwd(session_id),
            Provider::Gemini => resolve_gemini_session_cwd(session_id),
            Provider::Copilot | Provider::Vibe | Provider::Workflow => None,
        }
    }

    pub fn build_resume_args(
        &self,
        session_id: &str,
        prompt: &str,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts.and_then(|o| o.model.as_deref());
        let effort = opts.and_then(|o| o.effort.as_deref());

        match self {
            Provider::Claude => {
                let mut args = vec![
                    "--resume".into(),
                    session_id.into(),
                    "-p".into(),
                    prompt.into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--include-partial-messages".into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                if let Some(url) = transient_blackbox_url() {
                    let name = transient_blackbox_name();
                    args.extend(["--mcp-config".into(), claude_mcp_config_json(&name, &url)]);
                }
                args
            }
            Provider::Glm | Provider::Deepseek | Provider::Inception => {
                let mut args = vec![
                    "run".into(),
                    "--format".into(),
                    "json".into(),
                    "--session".into(),
                    session_id.into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--variant".into(), e.into()]);
                }
                args.push(prompt.into());
                args
            }
            Provider::Codex => {
                let mut args = vec![
                    "exec".into(),
                    "resume".into(),
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                    "--json".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
                }
                args.push(session_id.into());
                args.push(prompt.into());
                args
            }
            Provider::Copilot => {
                let mut args = vec![
                    "copilot".into(),
                    "--".into(),
                    format!("--resume={session_id}"),
                    "-p".into(),
                    prompt.into(),
                    "--yolo".into(),
                    "--autopilot".into(),
                    "--output-format".into(),
                    "json".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                args
            }
            Provider::Vibe => {
                // Vibe CLI has no `--model` flag — see build_exec_args.
                let _ = model;
                vec![
                    "--resume".into(),
                    session_id.into(),
                    "-p".into(),
                    prompt.into(),
                    "--output".into(),
                    "json".into(),
                ]
            }
            Provider::Gemini => {
                let mut args = vec![
                    "--resume".into(),
                    session_id.into(),
                    "-p".into(),
                    prompt.into(),
                    "--yolo".into(),
                    "-o".into(),
                    "json".into(),
                ];
                if let Some(m) = model {
                    args.extend(["--model".into(), m.into()]);
                }
                args
            }
            Provider::Workflow => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP registration + dispatch-time filters
// ---------------------------------------------------------------------------

use super::mcp::McpFilters;

impl Provider {
    /// Argv for `{provider} mcp add` registering an HTTP server.
    /// Returns None if the provider has no MCP CRUD CLI (Vibe).
    ///
    /// `exclude_tools` is honored only by Gemini (persistent, set at
    /// registration time). Other providers ignore it — they apply tool
    /// filtering per-dispatch via `build_filter_args`.
    pub fn build_mcp_add_http_args(
        &self,
        name: &str,
        url: &str,
        exclude_tools: &[String],
    ) -> Option<Vec<String>> {
        self.build_mcp_add_http_args_full(
            name,
            url,
            exclude_tools,
            &std::collections::BTreeMap::new(),
            "user",
        )
    }

    /// Scoped form. `scope` is "user" (global, default) or "project" (writes
    /// into the cwd's project-scoped config file). Returns None when the
    /// provider's CLI doesn't support that scope (Codex has no project
    /// scope; Vibe has no MCP CRUD at all).
    pub fn build_mcp_add_http_args_scoped(
        &self,
        name: &str,
        url: &str,
        exclude_tools: &[String],
        scope: &str,
    ) -> Option<Vec<String>> {
        self.build_mcp_add_http_args_full(
            name,
            url,
            exclude_tools,
            &std::collections::BTreeMap::new(),
            scope,
        )
    }

    /// Full form. Adds custom HTTP headers (e.g. auth tokens) to the
    /// add-args when the provider's CLI supports them: Claude (`-H`)
    /// and Gemini (`-H`). Codex's only header-like option is a bearer-
    /// token env var (`--bearer-token-env-var`) which can't be set
    /// from arbitrary key=value pairs without inventing env-var names,
    /// so Codex passes the URL alone — bearer tokens for Codex must
    /// be configured out-of-band. Copilot has no documented header
    /// flag. When the provider can't carry headers, they're silently
    /// dropped at the CLI layer (still persisted in McpServerConfig
    /// for clients that read the config directly).
    pub fn build_mcp_add_http_args_full(
        &self,
        name: &str,
        url: &str,
        exclude_tools: &[String],
        headers: &std::collections::BTreeMap<String, String>,
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
            Provider::Glm | Provider::Deepseek | Provider::Inception => None,
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
            Provider::Vibe | Provider::Workflow => None,
        }
    }

    /// Argv for `{provider} mcp remove <name>`. Scope-qualified so we
    /// only ever delete entries we wrote — never touch user-installed
    /// entries in other scopes.
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
            Provider::Glm | Provider::Deepseek | Provider::Inception => None,
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
            Provider::Vibe | Provider::Workflow => None,
        }
    }

    /// Argv for `{provider} mcp list` (stdout will differ per provider).
    pub fn build_mcp_list_args(&self) -> Option<Vec<String>> {
        match self {
            Provider::Claude => Some(vec!["mcp".into(), "list".into()]),
            Provider::Glm | Provider::Deepseek | Provider::Inception => None,
            Provider::Copilot => Some(vec![
                "copilot".into(),
                "--".into(),
                "mcp".into(),
                "list".into(),
            ]),
            Provider::Codex => Some(vec!["mcp".into(), "list".into()]),
            Provider::Gemini => Some(vec!["mcp".into(), "list".into()]),
            Provider::Vibe | Provider::Workflow => None,
        }
    }

    /// Detect whether `name` appears in a provider's `mcp list` output
    /// AND (optionally) whether its URL matches `expected_url`.
    ///
    /// Output formats differ: coarse substring match is sufficient for
    /// our "skip if present with matching URL, else upsert" flow.
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

    /// Argv SUFFIX appended to exec/resume for dispatch-time tool
    /// filters. Empty when the provider doesn't support such filtering
    /// (Vibe — no MCP) or the filter set is empty.
    ///
    /// Provider translation rules:
    ///   - Claude: pass glob patterns directly to `--disallowedTools` /
    ///     `--allowedTools` (native glob support).
    ///   - Copilot: pass glob patterns to repeated `--deny-tool=` /
    ///     `--allow-tool=` flags (native glob support).
    ///   - Codex: no glob support; expand current-blackbox-prefixed patterns
    ///     against the orchestration tool universe and emit a single
    ///     `-c mcp_servers.<blackbox-name>.disabled_tools=[...]` TOML override.
    ///     Patterns outside the blackbox namespace are skipped — Codex
    ///     can't filter per-tool outside its own MCP server model.
    ///   - Gemini: returns a placeholder; real policy file is generated
    ///     per-dispatch by the caller (see `write_gemini_policy_file`),
    ///     which appends `--policy <path>` to argv.
    pub fn build_filter_args(&self, filters: &McpFilters) -> Vec<String> {
        if filters.is_empty() {
            return Vec::new();
        }
        let mut args = Vec::new();
        match self {
            Provider::Claude => {
                // Claude's --disallowedTools matches tool names exactly
                // (or applies Bash-specific argument patterns inside
                // parentheses). It does NOT accept glob patterns on the
                // tool name itself. Expand the current blackbox MCP
                // prefix's `bro_*` pattern into
                // the concrete list of tool names so the filter fires.
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
            Provider::Glm | Provider::Deepseek | Provider::Inception => {}
            Provider::Copilot => {
                // Copilot's --deny-tool / --allow-tool expect
                // `ServerName(tool_name)` format, not the MCP-prefixed
                // form Claude accepts. Verified empirically: the
                // the current `mcp__<blackbox-name>__bro_status` form
                // passed through without blocking, while
                // `<blackbox-name>(bro_status)`
                // correctly denied the invocation.
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
            // Gemini: `--policy <path>` is appended by the caller after
            // generating the policy file. build_filter_args stays empty
            // so the caller knows whether to bother generating at all.
            Provider::Gemini => {}
            // Vibe has no MCP at all.
            Provider::Vibe | Provider::Workflow => {}
        }
        args
    }

    /// Whether this provider honors dispatch-time filters (vs registration-
    /// time or not at all). Claude, Copilot, Codex, and Gemini all
    /// support per-invocation mechanical filtering via different
    /// mechanisms. Only Vibe (no MCP) falls back to the text guard.
    pub fn supports_dispatch_filter(&self) -> bool {
        matches!(
            self,
            Provider::Claude | Provider::Copilot | Provider::Codex | Provider::Gemini
        )
    }
}

/// Group MCP-prefixed filter patterns by server name and emit one
/// `-c mcp_servers.<server>.<key>=[...]` arg per server. `key` is
/// `"disabled_tools"` for disallow filters and `"enabled_tools"` for
/// allow filters.
///
/// For the blackbox server we expand globs against the orchestration
/// tool universe (compile-time known). For other servers we don't have
/// a tool universe, so only exact-name patterns (no `*` / `?`) are
/// passed through; glob patterns on those are warned and skipped.
/// Non-MCP patterns (e.g. `Bash(...)`) are skipped — Codex's filter
/// scope is `mcp_servers.*` only.
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
    let universe: Vec<&str> = crate::tool_docs::orchestration_tool_names();
    let bb_prefix = crate::tool_docs::blackbox_mcp_prefix();
    let mut by_server: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for p in patterns
        .iter()
        .map(|p| super::mcp::normalize_filter_pattern(p))
    {
        let Some(rest) = p.strip_prefix("mcp__") else {
            tracing::debug!(target: "blackbox::filter",
                "codex skipping non-MCP pattern (filter scope is mcp_servers.*): {p}");
            continue;
        };
        let Some((server, tool_pat)) = rest.split_once("__") else {
            tracing::warn!(target: "blackbox::filter",
                "codex skipping malformed MCP pattern (expected mcp__<server>__<tool>): {p}");
            continue;
        };
        let group = by_server.entry(server.to_string()).or_default();
        if p.starts_with(&bb_prefix) {
            let expanded = super::mcp::expand_pattern(tool_pat, &universe);
            if expanded.is_empty() {
                tracing::warn!(target: "blackbox::filter",
                    "codex blackbox pattern matched zero tools (typo or stale name?): {p}");
                continue;
            }
            for t in expanded {
                if !group.contains(&t) {
                    group.push(t);
                }
            }
        } else if !tool_pat.contains('*') && !tool_pat.contains('?') {
            let t = tool_pat.to_string();
            if !group.contains(&t) {
                group.push(t);
            }
        } else {
            tracing::warn!(target: "blackbox::filter",
                "codex glob on non-blackbox server (no tool universe to expand against): {p}");
        }
    }
    by_server
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

/// Translate a `mcp__server__tool` full name into Copilot's
/// `Server(tool)` syntax. Returns None for patterns that aren't in
/// the MCP prefix form (e.g. `Bash(git *)` or `shell(git:*)`) so
/// callers can pass them through unchanged.
fn copilot_format_mcp_tool(full: &str) -> Option<String> {
    // Accept `mcp__<server>__<tool>` with the canonical double-
    // underscore separator.
    let rest = full.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some(format!("{server}({tool})"))
}

/// Expand filter patterns for providers that accept full MCP tool
/// names (Claude, Copilot). `mcp__<blackbox-name>__bro_*` style globs become
/// concrete `mcp__<blackbox-name>__bro_exec`, `mcp__<blackbox-name>__bro_resume`, …
/// entries. Non-blackbox patterns pass through unchanged — they're
/// likely already in a valid native form like `Bash(git push *)`.
fn expand_filter_patterns(patterns: &[String]) -> Vec<String> {
    let universe: Vec<&str> = crate::tool_docs::orchestration_tool_names();
    let prefix = crate::tool_docs::blackbox_mcp_prefix();
    let mut out = Vec::new();
    for p in patterns
        .iter()
        .map(|p| super::mcp::normalize_filter_pattern(p))
    {
        if let Some(stripped) = p.strip_prefix(&prefix) {
            for bare in super::mcp::expand_pattern(stripped, &universe) {
                let full = format!("{prefix}{bare}");
                if !out.contains(&full) {
                    out.push(full);
                }
            }
        } else if !out.contains(&p) {
            out.push(p.clone());
        }
    }
    out
}

/// Format a slice of strings as a TOML array literal (`["a", "b"]`)
/// for use inside `-c key=value` overrides. Each element is encoded as
/// a TOML basic string: escapes `\`, `"`, and all control chars
/// (0x00-0x1F + 0x7F) per the TOML 1.0 spec. Recognised whitespace
/// shorthands (`\t`, `\n`, `\r`) are preferred over `\uXXXX`.
fn format_toml_string_array(items: &[String]) -> String {
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

/// The daemon's own blackbox HTTP MCP URL, for transient injection
/// into dispatched provider CLIs. The daemon sets this once at startup
/// via `std::env::set_var("BLACKBOX_MCP_URL", ...)`. Using an env var
/// (vs threading through every arg-builder signature) keeps call-site
/// surface unchanged and stays consistent across exec/resume/broadcast
/// paths.
pub fn transient_blackbox_url() -> Option<String> {
    std::env::var("BLACKBOX_MCP_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn transient_blackbox_name() -> String {
    crate::util::blackbox_mcp_name()
}

/// Render the JSON payload for Claude's `--mcp-config` arg pointing at
/// the daemon's blackbox endpoint. Single entry — user's own MCP
/// servers are inherited additively (we don't pass `--strict-mcp-config`).
pub fn claude_mcp_config_json(name: &str, url: &str) -> String {
    serde_json::json!({
        "mcpServers": {
            name: { "type": "http", "url": url }
        }
    })
    .to_string()
}

/// Result of scanning a `mcp list` output for a specific entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchState {
    /// Name not found in output.
    Missing,
    /// Name found AND expected URL substring found (or no URL asked for).
    MatchesName,
    /// Name found but expected URL not found — registration drift.
    Drift,
}

// ---------------------------------------------------------------------------
// Event parsing — extract structured data from provider-specific JSON events
// ---------------------------------------------------------------------------

/// Mutable state that event parsing updates on a Task.
pub struct EventSink {
    pub last_assistant_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub session_id: Option<String>, // discovered session id
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Provider {
    /// Parse a streaming JSON event and update the sink.
    pub fn parse_event(&self, evt: &Value, sink: &mut EventSink) {
        match self {
            Provider::Claude => parse_claude_event(evt, sink),
            Provider::Glm | Provider::Deepseek | Provider::Inception => {
                parse_opencode_event(evt, sink)
            }
            Provider::Codex => parse_codex_event(evt, sink),
            Provider::Copilot => parse_copilot_event(evt, sink),
            Provider::Vibe => parse_vibe_event(evt, sink),
            Provider::Gemini => parse_gemini_event(evt, sink),
            Provider::Workflow => {}
        }
    }

    /// For non-streaming providers, parse the full stdout after process exit.
    pub fn parse_bulk_output(&self, raw: &str, sink: &mut EventSink) {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            self.parse_event(&parsed, sink);
        } else {
            sink.last_assistant_message = Some(raw.trim().to_string());
        }
    }

    pub fn build_export_args(&self, session_id: &str) -> Option<Vec<String>> {
        match self {
            Provider::Glm | Provider::Deepseek | Provider::Inception => {
                Some(vec!["export".into(), session_id.into()])
            }
            _ => None,
        }
    }
}

fn append_block_separator(buf: &mut Option<String>) {
    if let Some(existing) = buf.as_mut() {
        if !existing.is_empty() {
            existing.push_str("\n\n");
        }
    }
}

fn parse_claude_event(evt: &Value, sink: &mut EventSink) {
    // Hook events (subtype: hook_started / hook_response) carry their
    // own transient per-invocation session_id, distinct from the real
    // conversation session. They are emitted *before* the canonical
    // `subtype: init` event. Pulling session_id from a hook event
    // makes the streaming parser lock onto the hook id, then trip
    // the resume fork-detector when the real session_id arrives.
    // Skip them; only init + assistant/result events carry the
    // session_id worth tracking.
    if evt["type"].as_str() == Some("system") {
        let subtype = evt["subtype"].as_str();
        if matches!(subtype, Some("hook_started") | Some("hook_response")) {
            return;
        }
    }
    if let Some(session_id) = evt["session_id"]
        .as_str()
        .or_else(|| evt["sessionId"].as_str())
        .or_else(|| evt["message"]["session_id"].as_str())
        .or_else(|| evt["message"]["sessionId"].as_str())
    {
        sink.session_id = Some(session_id.to_string());
    }

    // Accumulate every assistant text block across the entire task.
    // Tool-using turns emit text alongside tool_use blocks; multi-turn
    // tool-use cycles emit text on more than one assistant message.
    // The previous design reset the buffer on each message_start and
    // overwrote on each assistant text block, which kept only the
    // final text — usually a brief closure like "No response
    // requested." — and dropped the actual answer that preceded the
    // tool call. text_delta and assistant text blocks now both append
    // (separated by a blank line between distinct blocks). thinking
    // and tool_use content do not contribute to the surfaced answer.
    if evt["type"].as_str() == Some("stream_event") {
        let inner_ty = evt["event"]["type"].as_str().unwrap_or("");
        match inner_ty {
            "content_block_start" => {
                if evt["event"]["content_block"]["type"].as_str() == Some("text") {
                    append_block_separator(&mut sink.last_assistant_message);
                }
            }
            "content_block_delta" => {
                if evt["event"]["delta"]["type"].as_str() == Some("text_delta") {
                    if let Some(chunk) = evt["event"]["delta"]["text"].as_str() {
                        let buf = sink.last_assistant_message.get_or_insert_with(String::new);
                        buf.push_str(chunk);
                    }
                }
            }
            _ => {}
        }
    }
    if evt["type"].as_str() == Some("assistant") {
        // The non-streaming `assistant` event mirrors a turn's full
        // content. With --include-partial-messages set on every Claude
        // dispatch (the canonical path), streaming events already
        // captured the text and this branch is a no-op. Without that
        // flag, fall back to populating from the assistant event so
        // text is still surfaced — appending each text block with a
        // blank-line separator preserves multi-block answers.
        let streaming_captured = sink
            .last_assistant_message
            .as_deref()
            .is_some_and(|m| !m.is_empty());
        if !streaming_captured {
            if let Some(content) = evt["message"]["content"].as_array() {
                for block in content {
                    if block["type"].as_str() == Some("text") {
                        if let Some(text) = block["text"].as_str() {
                            if text.is_empty() {
                                continue;
                            }
                            append_block_separator(&mut sink.last_assistant_message);
                            let buf = sink
                                .last_assistant_message
                                .get_or_insert_with(String::new);
                            buf.push_str(text);
                        }
                    }
                }
            }
        }
    }
    if evt["type"].as_str() == Some("result") {
        if let Some(result) = evt["result"].as_str() {
            // Claude's `result` summary field is the post-turn
            // single-shot answer when the turn produced exactly one
            // text block; it is empty when the turn ended with a
            // tool_use block, and reflects only the *final* text when
            // the turn contained multiple text blocks. Streaming
            // events have already captured every text block in order,
            // so prefer the accumulated streamed text over the
            // result summary. Only fall back to the summary when
            // streaming captured nothing.
            let already_have_streamed_text = sink
                .last_assistant_message
                .as_deref()
                .is_some_and(|m| !m.is_empty());
            if !result.is_empty() && !already_have_streamed_text {
                sink.last_assistant_message = Some(result.to_string());
            }
        }
        if let Some(usage) = evt["usage"].as_object() {
            sink.usage = Some(Usage {
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            });
        }
        sink.cost_usd = evt["total_cost_usd"].as_f64();
        sink.num_turns = evt["num_turns"].as_u64();
    }
}

fn parse_opencode_event(evt: &Value, sink: &mut EventSink) {
    if let Some(session_id) = evt["sessionID"].as_str() {
        sink.session_id = Some(session_id.to_string());
    }
    // Extract assistant text directly from the streaming `text` events
    // rather than relying solely on the post-run `opencode export`.
    // The export occasionally returns a stale or partial messages
    // array (observed: `messages` field present but the new turn's
    // assistant message not yet flushed to opencode's session DB by
    // the time export reads it), leaving last_assistant_message=None
    // and tripping the council drain's "empty body" filter.
    //
    // Streaming events are first-hand evidence of what the model
    // actually emitted. We OVERWRITE on every text event so the
    // final user-facing text wins (intermediate "let me check..."
    // texts in tool-using turns get superseded by the final answer
    // text in the next step). step_start resets so a fresh resume
    // turn doesn't carry text from a prior captured invocation.
    if evt["type"].as_str() == Some("step_start") {
        sink.last_assistant_message = None;
    } else if evt["type"].as_str() == Some("text") {
        if let Some(text) = evt["part"]["text"].as_str() {
            if !text.is_empty() {
                sink.last_assistant_message = Some(text.to_string());
            }
        }
    }
}

pub fn parse_opencode_export(raw: &str, sink: &mut EventSink) {
    let Some(json_start) = raw.find('{') else {
        return;
    };
    let Ok(export) = serde_json::from_str::<Value>(&raw[json_start..]) else {
        return;
    };

    if sink.session_id.is_none() {
        sink.session_id = export["info"]["id"].as_str().map(str::to_string);
    }

    let Some(messages) = export["messages"].as_array() else {
        return;
    };

    let assistant_messages: Vec<&Value> = messages
        .iter()
        .filter(|msg| msg["info"]["role"].as_str() == Some("assistant"))
        .collect();

    sink.num_turns = Some(assistant_messages.len() as u64);

    let Some(last_assistant) = assistant_messages.last() else {
        return;
    };

    let text_parts: Vec<&str> = last_assistant["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"].as_str() == Some("text"))
        .filter_map(|part| part["text"].as_str())
        .collect();
    if !text_parts.is_empty() {
        sink.last_assistant_message = Some(text_parts.join("\n"));
    }

    let input_tokens = last_assistant["info"]["tokens"]["input"].as_u64();
    let output_tokens = last_assistant["info"]["tokens"]["output"].as_u64();
    if let (Some(input_tokens), Some(output_tokens)) = (input_tokens, output_tokens) {
        sink.usage = Some(Usage {
            input_tokens,
            output_tokens,
        });
    }

    sink.cost_usd = last_assistant["info"]["cost"].as_f64();
}

fn parse_codex_event(evt: &Value, sink: &mut EventSink) {
    let msg_type = evt["type"].as_str().unwrap_or("");
    match msg_type {
        // item.completed — assistant message text
        "item.completed" => {
            if let Some(item) = evt.get("item") {
                if item["type"].as_str() == Some("agent_message") {
                    if let Some(text) = item["text"].as_str() {
                        sink.last_assistant_message = Some(text.to_string());
                    }
                }
            }
        }
        // turn.completed — usage stats
        "turn.completed" => {
            if let Some(usage) = evt["usage"].as_object() {
                sink.usage = Some(Usage {
                    input_tokens: usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    output_tokens: usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                });
            }
        }
        // thread.started — session discovery
        "thread.started" => {
            if let Some(tid) = evt["thread_id"].as_str() {
                sink.session_id = Some(tid.to_string());
            }
        }
        _ => {}
    }
}

fn parse_copilot_event(evt: &Value, sink: &mut EventSink) {
    let msg_type = evt["type"].as_str().unwrap_or("");
    match msg_type {
        // assistant.message — direct text responses
        "assistant.message" => {
            if let Some(data) = evt.get("data") {
                if let Some(content) = data["content"].as_str() {
                    sink.last_assistant_message = Some(content.to_string());
                }
            }
        }
        // session.task_complete — autopilot mode completion
        "session.task_complete" => {
            if let Some(data) = evt.get("data") {
                if let Some(summary) = data["summary"].as_str() {
                    sink.last_assistant_message = Some(summary.to_string());
                }
            }
        }
        // result — sessionId, usage
        "result" => {
            if let Some(sid) = evt["sessionId"].as_str() {
                sink.session_id = Some(sid.to_string());
            }
            if let Some(usage) = evt["usage"].as_object() {
                sink.usage = Some(Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                });
                sink.num_turns = usage.get("premiumRequests").and_then(|v| v.as_u64());
            }
        }
        _ => {}
    }
}

fn parse_vibe_event(evt: &Value, sink: &mut EventSink) {
    // Vibe returns bulk JSON on exit — an array of messages
    if let Some(arr) = evt.as_array() {
        // Find the last assistant message
        for msg in arr.iter().rev() {
            if msg["role"].as_str() == Some("assistant") {
                if let Some(content) = msg["content"].as_str() {
                    sink.last_assistant_message = Some(content.trim().to_string());
                    break;
                }
            }
        }
    }
}

fn parse_gemini_event(evt: &Value, sink: &mut EventSink) {
    // Gemini returns bulk JSON
    if let Some(response) = evt["response"].as_str() {
        sink.last_assistant_message = Some(response.to_string());
    }
    if let Some(session_id) = evt["session_id"].as_str() {
        sink.session_id = Some(session_id.to_string());
    }
    // Usage extraction from stats.models.*.tokens
    if let Some(stats) = evt.get("stats") {
        if let Some(models) = stats.get("models").and_then(|m| m.as_object()) {
            if let Some(first_model) = models.values().next() {
                if let Some(tokens) = first_model.get("tokens") {
                    sink.usage = Some(Usage {
                        input_tokens: tokens["input"].as_u64().unwrap_or(0),
                        output_tokens: tokens["candidates"].as_u64().unwrap_or(0),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vibe session discovery (post-hoc)
// ---------------------------------------------------------------------------

pub fn discover_vibe_session(start_ms: u64, project_dir: &str) -> Option<String> {
    let session_dir = std::env::var("VIBE_SESSION_DIR").unwrap_or_else(|_| {
        let home = dirs::home_dir().unwrap_or_default();
        home.join(".vibe/logs/session")
            .to_string_lossy()
            .to_string()
    });
    let session_path = Path::new(&session_dir);

    let entries: Vec<String> = match std::fs::read_dir(session_path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("session_"))
            .collect(),
        Err(_) => return None,
    };

    let resolved_project =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    let mut scored: Vec<(String, u64, bool, bool)> = entries
        .iter()
        .filter_map(|name| {
            let meta_file = session_path.join(name).join("meta.json");
            let stat = std::fs::metadata(&meta_file).ok()?;
            let mtime_ms = stat
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            let data: Value =
                serde_json::from_str(&std::fs::read_to_string(&meta_file).ok()?).ok()?;
            let env = data.get("environment")?.as_object()?;
            let wd = env.get("working_directory")?.as_str()?;

            let matches_dir = std::fs::canonicalize(wd)
                .map(|c| c == resolved_project)
                .unwrap_or(wd == project_dir);
            let recent = mtime_ms >= start_ms.saturating_sub(2000);
            let session_id = data["session_id"].as_str()?.to_string();

            Some((session_id, mtime_ms, matches_dir, recent))
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1)); // most recent first

    scored
        .iter()
        .find(|(_, _, dir, recent)| *dir && *recent)
        .or_else(|| scored.iter().find(|(_, _, dir, _)| *dir))
        .or_else(|| scored.iter().find(|(_, _, _, recent)| *recent))
        .map(|(sid, _, _, _)| sid.clone())
}

pub fn discover_gemini_session(
    start_ms: u64,
    project_dir: &str,
    task_id: Option<&str>,
) -> Option<String> {
    let tmp_root = dirs::home_dir()?.join(".gemini").join("tmp");
    discover_gemini_session_in(&tmp_root, start_ms, project_dir, task_id)
}

pub fn discover_gemini_session_in(
    tmp_root: &Path,
    start_ms: u64,
    project_dir: &str,
    task_id: Option<&str>,
) -> Option<String> {
    let resolved_project =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    let task_marker = task_id.map(|id| format!("task: {id}"));
    let mut scored: Vec<(String, u64, bool, bool, bool)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(tmp_root) else {
        return None;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let proj_dir = entry.path();
        if !proj_dir.is_dir() {
            continue;
        }
        let root_file = proj_dir.join(".project_root");
        let chats_dir = proj_dir.join("chats");
        if !root_file.exists() || !chats_dir.is_dir() {
            continue;
        }

        let Ok(root) = std::fs::read_to_string(&root_file) else {
            continue;
        };
        let root = root.trim();
        let matches_dir = std::fs::canonicalize(root)
            .map(|c| c == resolved_project)
            .unwrap_or_else(|_| Path::new(root) == resolved_project);

        let Ok(chats) = std::fs::read_dir(&chats_dir) else {
            continue;
        };
        for chat in chats.filter_map(|e| e.ok()) {
            let path = chat.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("session-") {
                continue;
            }
            let Ok(stat) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = stat.modified() else {
                continue;
            };
            let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) else {
                continue;
            };
            let mtime_ms = duration.as_millis() as u64;
            let recent = mtime_ms >= start_ms.saturating_sub(2000);
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let matches_task = task_marker
                .as_ref()
                .map(|marker| raw.contains(marker))
                .unwrap_or(false);
            let Ok(data) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let Some(session_id) = data["sessionId"].as_str() else {
                continue;
            };
            let session_id = session_id.to_string();
            scored.push((session_id, mtime_ms, matches_dir, recent, matches_task));
        }
    }

    scored.sort_by(|a, b| b.1.cmp(&a.1));
    let task_match = scored
        .iter()
        .find(|(_, _, dir, recent, task)| *dir && *recent && *task)
        .or_else(|| scored.iter().find(|(_, _, dir, _, task)| *dir && *task))
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, _, recent, task)| *recent && *task)
        })
        .or_else(|| scored.iter().find(|(_, _, _, _, task)| *task))
        .map(|(sid, _, _, _, _)| sid.clone());

    if task_id.is_some() {
        return task_match;
    }

    task_match
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, dir, recent, _)| *dir && *recent)
                .map(|(sid, _, _, _, _)| sid.clone())
        })
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, dir, _, _)| *dir)
                .map(|(sid, _, _, _, _)| sid.clone())
        })
        .or_else(|| {
            scored
                .iter()
                .find(|(_, _, _, recent, _)| *recent)
                .map(|(sid, _, _, _, _)| sid.clone())
        })
}

// ---------------------------------------------------------------------------
// Gemini session discovery (pre-dispatch cwd resolution)
// ---------------------------------------------------------------------------

/// Locate the project cwd recorded for a Gemini session UUID.
///
/// Gemini keys sessions per-cwd under `~/.gemini/tmp/<name>/chats/` with
/// filenames `session-<iso>-<first8>.json`. When `--resume <uuid>` runs
/// from a cwd whose folder doesn't contain that UUID, Gemini silently
/// forks a fresh session instead of erroring — which looks like a
/// successful resume at the daemon boundary but delivers the wrong
/// conversation context.
///
/// Returns the matched folder's `.project_root` path so the caller can
/// pin the child's cwd to what Gemini expects. `None` means no session
/// with this UUID is on disk — the caller should refuse the resume.
///
pub fn resolve_gemini_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    let tmp_root = dirs::home_dir()?.join(".gemini/tmp");
    resolve_gemini_session_cwd_in(&tmp_root, session_id)
}

// ---------------------------------------------------------------------------
// Claude session discovery
// ---------------------------------------------------------------------------

/// Locate the cwd a Claude session was recorded in.
///
/// Scans `~/.claude/projects/<slug>/<uuid>.jsonl` plus every sibling
/// `~/.claude-*/projects/...` that looks like a Claude account root, then
/// reads the cwd out of the first JSONL line that carries one. Lets
/// agents resume each other across repo boundaries without the caller
/// having to hand-pass the right `project_dir`.
pub fn resolve_claude_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    resolve_claude_session_cwd_in(&home, session_id)
}

pub fn resolve_claude_session_cwd_in(
    home: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    let mut accounts: Vec<std::path::PathBuf> = vec![home.join(".claude")];
    if let Ok(entries) = std::fs::read_dir(home) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(".claude-")
                && !name.contains("shared")
                && e.path().join("projects").exists()
            {
                accounts.push(e.path());
            }
        }
    }
    for account in &accounts {
        let projects = account.join("projects");
        let Ok(slugs) = std::fs::read_dir(&projects) else {
            continue;
        };
        for slug in slugs.flatten() {
            let path = slug.path().join(&file_name);
            if !path.is_file() {
                continue;
            }
            if let Some(cwd) = extract_claude_cwd(&path) {
                return Some(cwd);
            }
        }
    }
    None
}

fn extract_claude_cwd(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(f);
    for line in reader.lines().take(20).map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                if !cwd.is_empty() {
                    return Some(std::path::PathBuf::from(cwd));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Codex session discovery
// ---------------------------------------------------------------------------

/// Locate the cwd a Codex session was recorded in.
///
/// Codex stores sessions under `~/.codex/sessions/YYYY/MM/DD/
/// rollout-<iso>-<uuid>.jsonl` — cwd-independent storage, so the
/// resume itself would work from any working directory, but returning
/// the original cwd keeps tool resolution (path-based MCPs, git,
/// repo-local configs) correct after resurrection.
pub fn resolve_codex_session_cwd(session_id: &str) -> Option<std::path::PathBuf> {
    let root = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"));
    resolve_codex_session_cwd_in(&root, session_id)
}

pub fn resolve_codex_session_cwd_in(
    codex_root: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    let sessions = codex_root.join("sessions");
    let suffix = format!("-{session_id}.jsonl");
    // YYYY/MM/DD layout — walk three levels; filename match is cheap.
    let years = std::fs::read_dir(&sessions).ok()?;
    for year in years.flatten() {
        let Ok(months) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for month in months.flatten() {
            let Ok(days) = std::fs::read_dir(month.path()) else {
                continue;
            };
            for day in days.flatten() {
                let Ok(files) = std::fs::read_dir(day.path()) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if !name.starts_with("rollout-") || !name.ends_with(&suffix) {
                        continue;
                    }
                    if let Some(cwd) = extract_codex_cwd(&file.path()) {
                        return Some(cwd);
                    }
                }
            }
        }
    }
    None
}

fn extract_codex_cwd(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    v.get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

/// Testable form of `resolve_gemini_session_cwd` — scoped to an explicit
/// Gemini tmp root so unit tests can build a fixture without touching
/// `~/.gemini`.
pub fn resolve_gemini_session_cwd_in(
    tmp_root: &std::path::Path,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    use std::io::Read;
    if session_id.len() < 8 {
        return None;
    }
    let first8 = &session_id[..8];
    let suffix = format!("-{first8}.json");
    let needle = format!("\"sessionId\": \"{session_id}\"");

    for entry in std::fs::read_dir(tmp_root).ok()?.flatten() {
        let chats = entry.path().join("chats");
        let Ok(chat_entries) = std::fs::read_dir(&chats) else {
            continue;
        };
        for chat in chat_entries.flatten() {
            let name = chat.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("session-") || !name.ends_with(&suffix) {
                continue;
            }
            // The filename suffix is only the UUID's first 8 chars, so
            // confirm the full sessionId via the file header before
            // trusting the match.
            let Ok(mut f) = std::fs::File::open(chat.path()) else {
                continue;
            };
            let mut buf = [0u8; 256];
            let n = f.read(&mut buf).ok()?;
            let header = std::str::from_utf8(&buf[..n]).unwrap_or("");
            if !header.contains(&needle) {
                continue;
            }

            let Ok(root) = std::fs::read_to_string(entry.path().join(".project_root")) else {
                continue;
            };
            let root = root.trim();
            if root.is_empty() {
                continue;
            }
            return Some(std::path::PathBuf::from(root));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Model/Effort catalogs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffortInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
}

static CLAUDE_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "low",
        description: "Light reasoning",
        default: false,
    },
    EffortInfo {
        id: "medium",
        description: "Balanced speed and depth",
        default: false,
    },
    EffortInfo {
        id: "high",
        description: "Greater depth for complex problems",
        default: false,
    },
    EffortInfo {
        id: "xhigh",
        description: "Extended depth (Opus 4.7 only)",
        default: true,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth",
        default: false,
    },
];

static OPENCODE_VARIANTS: &[EffortInfo] = &[
    EffortInfo {
        id: "minimal",
        description: "Fastest variant",
        default: false,
    },
    EffortInfo {
        id: "low",
        description: "Light reasoning",
        default: false,
    },
    EffortInfo {
        id: "medium",
        description: "Balanced speed and depth",
        default: true,
    },
    EffortInfo {
        id: "high",
        description: "Deeper reasoning",
        default: false,
    },
    EffortInfo {
        id: "max",
        description: "Maximum reasoning depth",
        default: false,
    },
];

static CLAUDE_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-4-7",
        description: "Frontier model, 1M context built-in",
        default: true,
    },
    ModelInfo {
        id: "claude-opus-4-6[1m]",
        description: "Previous frontier, 1M context window",
        default: false,
    },
    ModelInfo {
        id: "claude-opus-4-6",
        description: "Previous frontier, 200K context",
        default: false,
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        description: "Fast + capable, balanced cost",
        default: false,
    },
    ModelInfo {
        id: "claude-haiku-4-5-20251001",
        description: "Fastest, lowest cost",
        default: false,
    },
];

static GLM_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "zai-coding-plan/glm-5.1",
        description: "Z.AI Coding Plan flagship GLM model via OpenCode",
        default: true,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-5",
        description: "General-purpose frontier GLM model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-5-turbo",
        description: "Fast high-end GLM model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.7",
        description: "Strong balanced GLM model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.7-flashx",
        description: "Cheap accelerated GLM-4.7 variant via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.6",
        description: "Previous balanced GLM model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.5",
        description: "Balanced GLM-4.5 model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.5-air",
        description: "Low-cost helper model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.5v",
        description: "Vision-capable GLM-4.5 model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.6v",
        description: "Vision-capable GLM-4.6 model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.7-flash",
        description: "Free GLM-4.7 flash model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-4.5-flash",
        description: "Free GLM flash model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "zai-coding-plan/glm-5v-turbo",
        description: "Vision-capable GLM model via OpenCode",
        default: false,
    },
];

static DEEPSEEK_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "deepseek/deepseek-v4-pro",
        description: "DeepSeek 4.1 Pro / V4 Pro reasoning model via OpenCode",
        default: true,
    },
    ModelInfo {
        id: "deepseek/deepseek-v4-flash",
        description: "Fast DeepSeek V4 model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "deepseek/deepseek-reasoner",
        description: "DeepSeek reasoning model via OpenCode",
        default: false,
    },
    ModelInfo {
        id: "deepseek/deepseek-chat",
        description: "DeepSeek chat model via OpenCode",
        default: false,
    },
];

static INCEPTION_MODELS: &[ModelInfo] = &[ModelInfo {
    id: "inception/mercury-2",
    description: "Inception Mercury 2 tool-capable model via OpenCode",
    default: true,
}];

static CODEX_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gpt-5.5",
        description: "Latest frontier agentic coding model (subsumes codex flavor)",
        default: true,
    },
    ModelInfo {
        id: "gpt-5.5-mini",
        description: "Smaller 5.5-family model (API-direct only; not available on ChatGPT account)",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.4",
        description: "Prior-generation frontier agentic coding model (subsumes codex flavor)",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.4-mini",
        description: "Smaller 5.4-family model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        description: "Frontier Codex-optimized agentic coding model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex-spark",
        description: "Ultra-fast coding model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2-codex",
        description: "Frontier agentic coding model",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2",
        description: "Optimized for professional work and long-running agents",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.1-codex-max",
        description: "Deep and fast reasoning, xhigh effort",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.1-codex-mini",
        description: "Cheaper, faster, less capable",
        default: false,
    },
];

static CODEX_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "minimal",
        description: "Fastest, fewest reasoning tokens",
        default: false,
    },
    EffortInfo {
        id: "low",
        description: "Light reasoning",
        default: false,
    },
    EffortInfo {
        id: "medium",
        description: "Balanced speed and depth",
        default: true,
    },
    EffortInfo {
        id: "high",
        description: "Greater depth for complex problems",
        default: false,
    },
    EffortInfo {
        id: "xhigh",
        description: "Maximum depth (gpt-5.4 / gpt-5.5 / gpt-5.1-codex-max / gpt-5.2-codex)",
        default: false,
    },
];

static COPILOT_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "claude-opus-4-7",
        description: "Anthropic Opus 4.7",
        default: true,
    },
    ModelInfo {
        id: "claude-opus-4-6",
        description: "Anthropic Opus 4.6",
        default: false,
    },
    ModelInfo {
        id: "claude-sonnet-4-6",
        description: "Anthropic Sonnet 4.6",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.3-codex",
        description: "OpenAI Codex-optimized",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2-codex",
        description: "OpenAI Codex",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.1-codex-max",
        description: "OpenAI deep reasoning",
        default: false,
    },
    ModelInfo {
        id: "gpt-5.2",
        description: "OpenAI general purpose",
        default: false,
    },
];

static COPILOT_EFFORTS: &[EffortInfo] = &[
    EffortInfo {
        id: "low",
        description: "Fast responses with lighter reasoning",
        default: false,
    },
    EffortInfo {
        id: "medium",
        description: "Balanced speed and depth",
        default: true,
    },
    EffortInfo {
        id: "high",
        description: "Greater depth for complex problems",
        default: false,
    },
    EffortInfo {
        id: "xhigh",
        description: "Maximum reasoning depth",
        default: false,
    },
];

// Vibe CLI does not expose per-invocation model selection (no --model
// flag). Model is configured out-of-band via `--agent NAME`
// (~/.vibe/agents/*.toml) or `vibe --setup`. Listing models here would
// imply they're selectable through bro_exec/brofiles when they aren't.
static VIBE_MODELS: &[ModelInfo] = &[];

static GEMINI_MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "gemini-3.1-pro-preview",
        description: "Gemini 3.1 Pro, flagship reasoning model (preview)",
        default: true,
    },
    ModelInfo {
        id: "gemini-3-flash-preview",
        description: "Gemini 3 Flash, fast generalist (preview)",
        default: false,
    },
    ModelInfo {
        id: "gemini-3.1-flash-lite-preview",
        description: "Gemini 3.1 Flash-Lite, lowest cost (preview)",
        default: false,
    },
    ModelInfo {
        id: "gemini-2.5-pro",
        description: "Gemini 2.5 Pro, prior-gen flagship (GA)",
        default: false,
    },
    ModelInfo {
        id: "gemini-2.5-flash",
        description: "Gemini 2.5 Flash, prior-gen fast (GA)",
        default: false,
    },
    ModelInfo {
        id: "gemini-2.5-flash-lite",
        description: "Gemini 2.5 Flash-Lite, prior-gen low-cost (GA)",
        default: false,
    },
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_provider_roundtrip() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_str(p.as_str()).ok(), Some(*p));
        }
        assert_eq!(Provider::from_str("opencode").ok(), Some(Provider::Glm));
        assert!(Provider::from_str("unknown").is_err());
    }

    #[test]
    fn test_inception_catalog_exposes_only_tool_capable_mercury() {
        let models = Provider::Inception.models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "inception/mercury-2");
        assert!(models[0].default);
        assert!(!models.iter().any(|m| m.id == "inception/mercury-edit-2"));
    }

    #[test]
    fn test_claude_exec_args() {
        let args = Provider::Claude.build_exec_args("hello", "sid-1", None, None);
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"hello".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
    }

    #[test]
    fn test_claude_resume_args() {
        let args = Provider::Claude.build_resume_args("sid-1", "follow up", None);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"follow up".to_string()));
    }

    #[test]
    fn test_codex_exec_args_with_effort() {
        let opts = ExecOpts {
            model: Some("gpt-5.4".into()),
            effort: Some("high".into()),
        };
        let args = Provider::Codex.build_exec_args("do stuff", "", None, Some(&opts));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"gpt-5.4".to_string()));
        assert!(args.iter().any(|a| a.contains("model_reasoning_effort")));
    }

    #[test]
    fn test_codex_exec_args_with_cwd() {
        let args = Provider::Codex.build_exec_args("task", "", Some("/tmp/proj"), None);
        assert!(args.contains(&"-C".to_string()));
        assert!(args.contains(&"/tmp/proj".to_string()));
    }

    #[test]
    fn test_gemini_resume_args() {
        let args = Provider::Gemini.build_resume_args("gsid-1", "continue", None);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"gsid-1".to_string()));
        assert!(args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn test_copilot_exec_args() {
        let args = Provider::Copilot.build_exec_args("review this", "", None, None);
        assert_eq!(args[0], "copilot");
        assert_eq!(args[1], "--");
        assert!(args.contains(&"--autopilot".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
    }

    #[test]
    fn test_vibe_resume_args() {
        let args = Provider::Vibe.build_resume_args("s1", "continue", None);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"s1".to_string()));
        assert!(args.contains(&"--output".to_string()));
    }

    #[test]
    fn test_vibe_ignores_model_param() {
        let opts = ExecOpts {
            model: Some("devstral-2".into()),
            effort: None,
        };
        let exec_args = Provider::Vibe.build_exec_args("hi", "sid", None, Some(&opts));
        assert!(
            !exec_args.contains(&"--model".to_string()),
            "vibe exec must not emit --model (CLI rejects it): {exec_args:?}"
        );
        let resume_args = Provider::Vibe.build_resume_args("sid", "hi", Some(&opts));
        assert!(
            !resume_args.contains(&"--model".to_string()),
            "vibe resume must not emit --model (CLI rejects it): {resume_args:?}"
        );
    }

    #[test]
    fn test_streaming_json_classification() {
        assert!(Provider::Claude.is_streaming_json());
        assert!(Provider::Codex.is_streaming_json());
        assert!(Provider::Copilot.is_streaming_json());
        assert!(!Provider::Vibe.is_streaming_json());
        assert!(!Provider::Gemini.is_streaming_json());
    }

    #[test]
    fn test_parse_claude_result_event() {
        let evt = serde_json::json!({
            "type": "result",
            "result": "The answer is 42",
            "usage": { "input_tokens": 100, "output_tokens": 50 },
            "total_cost_usd": 0.05,
            "num_turns": 3
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Claude.parse_event(&evt, &mut sink);
        assert_eq!(
            sink.last_assistant_message.as_deref(),
            Some("The answer is 42")
        );
        assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 100);
        assert_eq!(sink.cost_usd, Some(0.05));
        assert_eq!(sink.num_turns, Some(3));
    }

    #[test]
    fn test_parse_claude_streaming_accumulates_text_across_blocks_and_turns() {
        // Tool-using turns interleave text blocks and tool_use blocks;
        // multi-turn loops emit text on more than one assistant message.
        // Streaming must accumulate every text block (separated by a
        // blank line) so the substantive answer is not clobbered by a
        // trailing closure like "No response requested." that arrives
        // in a later block / later turn.
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        let events = vec![
            serde_json::json!({"type":"stream_event","event":{"type":"message_start"}}),
            serde_json::json!({
                "type":"stream_event",
                "event":{"type":"content_block_start","content_block":{"type":"text"}}
            }),
            serde_json::json!({
                "type":"stream_event",
                "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Substantive answer."}}
            }),
            serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}),
            serde_json::json!({
                "type":"stream_event",
                "event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"some_tool"}}
            }),
            serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}),
            // Turn 2 begins; message_start no longer resets.
            serde_json::json!({"type":"stream_event","event":{"type":"message_start"}}),
            serde_json::json!({
                "type":"stream_event",
                "event":{"type":"content_block_start","content_block":{"type":"text"}}
            }),
            serde_json::json!({
                "type":"stream_event",
                "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"No response requested."}}
            }),
            serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop"}}),
            // Result event with empty `result` (turn ended on a tool_use earlier);
            // must not clobber accumulated streamed text.
            serde_json::json!({"type":"result","result":""}),
        ];
        for evt in &events {
            Provider::Claude.parse_event(evt, &mut sink);
        }
        assert_eq!(
            sink.last_assistant_message.as_deref(),
            Some("Substantive answer.\n\nNo response requested."),
            "streaming must accumulate every text block across turns"
        );
    }

    #[test]
    fn test_parse_claude_result_with_empty_text_preserves_streamed_message() {
        // When a Claude turn ends with a tool_use block, the post-turn
        // `result` event's `result` field is the empty string (the
        // user-facing answer text was emitted earlier as its own block
        // and captured by the streaming parser). The result event
        // must not clobber that captured text with empty.
        let stream_text = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Captured during the turn." }
                ]
            }
        });
        let result_with_empty = serde_json::json!({
            "type": "result",
            "result": "",
            "usage": { "input_tokens": 10, "output_tokens": 50 },
            "total_cost_usd": 0.001,
            "num_turns": 1
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Claude.parse_event(&stream_text, &mut sink);
        assert_eq!(
            sink.last_assistant_message.as_deref(),
            Some("Captured during the turn.")
        );
        Provider::Claude.parse_event(&result_with_empty, &mut sink);
        assert_eq!(
            sink.last_assistant_message.as_deref(),
            Some("Captured during the turn."),
            "empty result must not overwrite previously captured text"
        );
        // Usage / cost / num_turns from the result event should still apply.
        assert_eq!(sink.cost_usd, Some(0.001));
        assert_eq!(sink.num_turns, Some(1));
        assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 50);
    }

    #[test]
    fn test_parse_claude_hook_event_skips_session_capture() {
        // Hook events (subtype: hook_started/hook_response) carry a
        // transient per-invocation session_id distinct from the
        // canonical conversation session. They land before the real
        // `init` event; if the parser reads session_id from them, the
        // streaming sink locks onto the hook id and the resume
        // fork-detector trips when the real session_id arrives.
        let hook_started = serde_json::json!({
            "type": "system",
            "subtype": "hook_started",
            "session_id": "hook-only-id"
        });
        let hook_response = serde_json::json!({
            "type": "system",
            "subtype": "hook_response",
            "session_id": "hook-only-id"
        });
        let init = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "real-conversation-id"
        });

        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Claude.parse_event(&hook_started, &mut sink);
        assert_eq!(sink.session_id, None, "hook_started must not set session_id");
        Provider::Claude.parse_event(&hook_response, &mut sink);
        assert_eq!(sink.session_id, None, "hook_response must not set session_id");
        Provider::Claude.parse_event(&init, &mut sink);
        assert_eq!(
            sink.session_id.as_deref(),
            Some("real-conversation-id"),
            "init event must set session_id"
        );
    }

    #[test]
    fn test_parse_claude_assistant_event() {
        let evt = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Working on it..." }
                ]
            }
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Claude.parse_event(&evt, &mut sink);
        assert_eq!(
            sink.last_assistant_message.as_deref(),
            Some("Working on it...")
        );
    }

    #[test]
    fn test_parse_codex_thread_started_event() {
        let evt = serde_json::json!({
            "type": "thread.started",
            "thread_id": "codex-thread-123"
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Codex.parse_event(&evt, &mut sink);
        assert_eq!(sink.session_id.as_deref(), Some("codex-thread-123"));
    }

    #[test]
    fn test_parse_codex_item_completed_event() {
        let evt = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "agent_message", "text": "Done!" }
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Codex.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("Done!"));
    }

    #[test]
    fn test_parse_codex_turn_completed_event() {
        let evt = serde_json::json!({
            "type": "turn.completed",
            "usage": { "input_tokens": 200, "output_tokens": 80 }
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Codex.parse_event(&evt, &mut sink);
        assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 200);
        assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 80);
    }

    #[test]
    fn test_parse_copilot_assistant_message() {
        let evt = serde_json::json!({
            "type": "assistant.message",
            "data": { "content": "Here's the fix" }
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Copilot.parse_event(&evt, &mut sink);
        assert_eq!(
            sink.last_assistant_message.as_deref(),
            Some("Here's the fix")
        );
    }

    #[test]
    fn test_parse_copilot_result_event() {
        let evt = serde_json::json!({
            "type": "result",
            "sessionId": "copilot-sid",
            "usage": { "premiumRequests": 5 }
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Copilot.parse_event(&evt, &mut sink);
        assert_eq!(sink.session_id.as_deref(), Some("copilot-sid"));
        assert_eq!(sink.num_turns, Some(5));
    }

    #[test]
    fn test_parse_vibe_array_event() {
        let evt = serde_json::json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "  Hi there!  "},
            {"role": "assistant", "content": "  Final answer  "}
        ]);
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Vibe.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("Final answer"));
    }

    #[test]
    fn test_parse_gemini_with_stats() {
        let evt = serde_json::json!({
            "response": "The answer",
            "session_id": "gem-sid",
            "stats": {
                "models": {
                    "gemini-2.5-flash": {
                        "tokens": { "input": 150, "candidates": 60 }
                    }
                }
            }
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Gemini.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("The answer"));
        assert_eq!(sink.session_id.as_deref(), Some("gem-sid"));
        assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 150);
        assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 60);
    }

    #[test]
    fn test_models_nonempty() {
        for p in Provider::ALL {
            // Vibe has no selectable model surface (CLI lacks --model);
            // catalog is intentionally empty.
            if matches!(p, Provider::Vibe) {
                continue;
            }
            assert!(
                !p.models().is_empty(),
                "{} should have at least one model",
                p
            );
        }
    }

    #[test]
    fn test_each_provider_has_default_model() {
        for p in Provider::ALL {
            if matches!(p, Provider::Vibe) {
                continue;
            }
            let has_default = p.models().iter().any(|m| m.default);
            assert!(has_default, "{} should have a default model", p);
        }
    }

    #[test]
    fn test_vibe_models_empty() {
        assert!(
            Provider::Vibe.models().is_empty(),
            "vibe must not advertise selectable models — CLI has no --model flag"
        );
    }

    #[test]
    fn test_mcp_add_args_shape_per_provider() {
        let u = "http://127.0.0.1:7264/mcp";
        let c = Provider::Claude
            .build_mcp_add_http_args("blackbox", u, &[])
            .unwrap();
        assert_eq!(&c[..4], &["mcp", "add", "-s", "user"]);
        assert!(c.contains(&"--transport".to_string()));
        assert!(c.contains(&"http".to_string()));
        assert!(c.contains(&"blackbox".to_string()));
        assert!(c.contains(&u.to_string()));

        let co = Provider::Copilot
            .build_mcp_add_http_args("blackbox", u, &[])
            .unwrap();
        assert!(co.starts_with(&["copilot".to_string(), "--".to_string()]));
        assert!(co.contains(&"--transport".to_string()));

        let cx = Provider::Codex
            .build_mcp_add_http_args("blackbox", u, &[])
            .unwrap();
        assert!(cx.contains(&"--url".to_string()));
        assert!(cx.contains(&u.to_string()));

        let g = Provider::Gemini
            .build_mcp_add_http_args("blackbox", u, &[])
            .unwrap();
        assert!(g.iter().any(|a| a == "-t"));
        assert!(g.iter().any(|a| a == "-s"));
        assert!(g.contains(&u.to_string()));

        assert!(Provider::Vibe
            .build_mcp_add_http_args("x", "y", &[])
            .is_none());
    }

    #[test]
    fn test_gemini_mcp_add_includes_exclude_tools() {
        let exclude = vec!["bro_exec".to_string(), "bro_resume".to_string()];
        let args = Provider::Gemini
            .build_mcp_add_http_args("blackbox", "http://x/mcp", &exclude)
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("--exclude-tools"));
        assert!(joined.contains("bro_exec,bro_resume"));
    }

    #[test]
    fn test_mcp_list_has_detects_states() {
        let out =
            "Name        URL\nblackbox    http://127.0.0.1:7264/mcp\nother       http://x/mcp\n";
        assert_eq!(
            Provider::Claude.mcp_list_has(out, "blackbox", Some("http://127.0.0.1:7264/mcp")),
            MatchState::MatchesName
        );
        assert_eq!(
            Provider::Claude.mcp_list_has(out, "blackbox", Some("http://127.0.0.1:9999/mcp")),
            MatchState::Drift
        );
        assert_eq!(
            Provider::Claude.mcp_list_has(out, "absent", None),
            MatchState::Missing
        );
    }

    #[test]
    fn test_claude_filter_disallow_args_expands_blackbox_globs() {
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__.bro_*".into(), "Bash(rm -rf *)".into()],
            allow: vec![],
        };
        let args = Provider::Claude.build_filter_args(&filters);
        assert_eq!(args[0], "--disallowedTools");
        // Glob expanded to concrete tool names.
        assert!(args[1].contains("mcp__blackbox__bro_exec"));
        assert!(args[1].contains("mcp__blackbox__bro_resume"));
        // Non-blackbox pattern passes through unchanged.
        assert!(args[1].contains("Bash(rm -rf *)"));
        // The raw glob should NOT appear — it'd be treated as a literal
        // tool name by Claude and match nothing.
        assert!(!args[1]
            .split_whitespace()
            .any(|t| t == "mcp__blackbox__bro_*"));
    }

    #[test]
    fn test_copilot_filter_repeats_flag_expanded() {
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__.bro_*".into(), "shell(git push)".into()],
            allow: vec!["shell".into()],
        };
        let args = Provider::Copilot.build_filter_args(&filters);
        // Each expanded bro_* tool translates to Copilot's
        // `Server(tool)` syntax, not the MCP prefix form.
        assert!(args.iter().any(|a| a == "--deny-tool=blackbox(bro_exec)"));
        assert!(args.iter().any(|a| a == "--deny-tool=blackbox(bro_resume)"));
        // No mcp__ prefix leaks into copilot args.
        assert!(!args.iter().any(|a| a.contains("mcp__blackbox__")));
        // Non-MCP patterns (shell(...) native form) pass through.
        assert!(args.contains(&"--deny-tool=shell(git push)".to_string()));
        assert!(args.contains(&"--allow-tool=shell".to_string()));
    }

    #[test]
    fn test_copilot_format_mcp_tool_translation() {
        assert_eq!(
            copilot_format_mcp_tool("mcp__blackbox__bro_exec"),
            Some("blackbox(bro_exec)".to_string())
        );
        assert_eq!(
            copilot_format_mcp_tool("mcp__foo__bar"),
            Some("foo(bar)".to_string())
        );
        // Not MCP-shaped → None, caller uses original.
        assert_eq!(copilot_format_mcp_tool("Bash(git *)"), None);
        assert_eq!(copilot_format_mcp_tool("mcp__only_one_underscore"), None);
    }

    #[test]
    fn test_codex_expands_blackbox_glob_to_disabled_tools() {
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__.bro_*".into()],
            allow: vec![],
        };
        let args = Provider::Codex.build_filter_args(&filters);
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("mcp_servers.blackbox.disabled_tools=["));
        // Should contain at least the core bro_* names.
        assert!(args[1].contains("bro_exec"));
        assert!(args[1].contains("bro_resume"));
        assert!(args[1].contains("bro_mcp"));
        // Should NOT contain any bbox_* tools (different category).
        assert!(!args[1].contains("bbox_note"));
    }

    #[test]
    fn test_codex_skips_non_mcp_patterns() {
        let filters = McpFilters {
            disallow: vec!["Bash(git push *)".into()],
            allow: vec![],
        };
        let args = Provider::Codex.build_filter_args(&filters);
        // Codex's filter scope is mcp_servers.* — patterns outside the
        // MCP namespace (Bash, shell, etc.) produce no args.
        assert!(args.is_empty());
    }

    #[test]
    fn test_codex_routes_non_blackbox_mcp_pattern_to_correct_server() {
        let filters = McpFilters {
            disallow: vec!["mcp__github__.create_issue".into()],
            allow: vec![],
        };
        let args = Provider::Codex.build_filter_args(&filters);
        // Exact tool name on a non-blackbox MCP server routes to that
        // server's disabled_tools array.
        assert_eq!(args[0], "-c");
        assert_eq!(
            args[1],
            "mcp_servers.github.disabled_tools=[\"create_issue\"]"
        );
    }

    #[test]
    fn test_codex_warns_on_glob_against_unknown_server() {
        // Glob against a non-blackbox server can't be expanded (no tool
        // universe), so it's skipped with a warning. End result: empty.
        let filters = McpFilters {
            disallow: vec!["mcp__github__.create_*".into()],
            allow: vec![],
        };
        let args = Provider::Codex.build_filter_args(&filters);
        assert!(args.is_empty());
    }

    #[test]
    fn test_codex_emits_enabled_tools_for_allow() {
        let filters = McpFilters {
            disallow: vec![],
            allow: vec!["mcp__blackbox__bro_status".into()],
        };
        let args = Provider::Codex.build_filter_args(&filters);
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("mcp_servers.blackbox.enabled_tools=["));
        assert!(args[1].contains("bro_status"));
    }

    #[test]
    fn test_codex_groups_multiple_servers_into_separate_overrides() {
        let filters = McpFilters {
            disallow: vec![
                "mcp__blackbox__bro_exec".into(),
                "mcp__github__create_issue".into(),
            ],
            allow: vec![],
        };
        let args = Provider::Codex.build_filter_args(&filters);
        // Two `-c` overrides — one per server. BTreeMap iteration is
        // alphabetical, so blackbox comes before github.
        let overrides: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("mcp_servers."))
            .collect();
        assert_eq!(overrides.len(), 2);
        assert!(overrides[0].starts_with("mcp_servers.blackbox.disabled_tools="));
        assert!(overrides[1].starts_with("mcp_servers.github.disabled_tools="));
    }

    #[test]
    fn test_gemini_filter_args_deferred_to_policy_file() {
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__.bro_*".into()],
            allow: vec![],
        };
        // Gemini gets its policy via --policy <file>, produced by the
        // caller. build_filter_args returns empty so callers know to
        // handle it separately.
        assert!(Provider::Gemini.build_filter_args(&filters).is_empty());
    }

    #[test]
    fn test_vibe_ignores_filters() {
        let filters = McpFilters {
            disallow: vec!["anything".into()],
            allow: vec![],
        };
        assert!(Provider::Vibe.build_filter_args(&filters).is_empty());
    }

    #[test]
    fn test_supports_dispatch_filter_all_but_vibe() {
        assert!(Provider::Claude.supports_dispatch_filter());
        assert!(Provider::Copilot.supports_dispatch_filter());
        assert!(Provider::Codex.supports_dispatch_filter());
        assert!(Provider::Gemini.supports_dispatch_filter());
        assert!(!Provider::Vibe.supports_dispatch_filter());
    }

    #[test]
    fn test_format_toml_string_array() {
        assert_eq!(format_toml_string_array(&[]), "[]");
        assert_eq!(
            format_toml_string_array(&["a".into(), "b".into()]),
            r#"["a","b"]"#
        );
        assert_eq!(
            format_toml_string_array(&[r#"with"quote"#.into()]),
            r#"["with\"quote"]"#
        );
    }

    #[test]
    fn test_build_mcp_add_http_args_full_threads_headers() {
        use std::collections::BTreeMap;
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer xyz".to_string());
        headers.insert("X-Trace".to_string(), "abc123".to_string());

        // Claude emits -H "key: value" pairs.
        let claude = Provider::Claude
            .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
            .unwrap();
        let joined = claude.join(" | ");
        assert!(
            joined.contains("-H | Authorization: Bearer xyz"),
            "got: {joined}"
        );
        assert!(joined.contains("-H | X-Trace: abc123"), "got: {joined}");

        // Gemini also emits -H pairs.
        let gemini = Provider::Gemini
            .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
            .unwrap();
        let joined = gemini.join(" | ");
        assert!(joined.contains("-H | Authorization: Bearer xyz"));

        // Codex drops headers (only --bearer-token-env-var supported).
        let codex = Provider::Codex
            .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
            .unwrap();
        assert!(!codex.iter().any(|a| a == "-H"));
        assert!(!codex.iter().any(|a| a.contains("Bearer xyz")));

        // Copilot drops headers (no documented header flag).
        let copilot = Provider::Copilot
            .build_mcp_add_http_args_full("blackbox", "http://x/mcp", &[], &headers, "user")
            .unwrap();
        assert!(!copilot.iter().any(|a| a == "-H"));
    }

    #[test]
    fn test_scoped_arg_builders_honor_scope_capability() {
        // Claude + Gemini support both user and project.
        assert!(Provider::Claude
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_some());
        assert!(Provider::Claude
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_some());
        assert!(Provider::Gemini
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_some());

        // Codex has no project scope (single config file).
        assert!(Provider::Codex
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_some());
        assert!(Provider::Codex
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none());
        assert!(Provider::Codex
            .build_mcp_remove_args_scoped("x", "project")
            .is_none());

        // Copilot only user (no documented project flag).
        assert!(Provider::Copilot
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none());

        // Vibe never.
        assert!(Provider::Vibe
            .build_mcp_add_http_args_scoped("x", "u", &[], "user")
            .is_none());
        assert!(Provider::Vibe
            .build_mcp_add_http_args_scoped("x", "u", &[], "project")
            .is_none());

        // Claude project scope emits -s project.
        let claude_proj = Provider::Claude
            .build_mcp_add_http_args_scoped("x", "http://u/mcp", &[], "project")
            .unwrap();
        let joined = claude_proj.join(" ");
        assert!(
            joined.contains("-s project"),
            "expected -s project in: {joined}"
        );
        // Gemini project scope emits -s project.
        let gemini_proj = Provider::Gemini
            .build_mcp_add_http_args_scoped("x", "http://u/mcp", &[], "project")
            .unwrap();
        assert!(gemini_proj.join(" ").contains("-s project"));
    }

    #[test]
    fn test_format_toml_string_array_escapes_control_chars() {
        // TOML basic strings forbid raw control chars (0x00-0x1F + 0x7F).
        // Recognised shortforms preferred; everything else \uXXXX.
        assert_eq!(format_toml_string_array(&["a\tb".into()]), r#"["a\tb"]"#);
        assert_eq!(
            format_toml_string_array(&["x\ny\rz".into()]),
            r#"["x\ny\rz"]"#
        );
        assert_eq!(
            format_toml_string_array(&["\x00null".into()]),
            r#"["\u0000null"]"#
        );
        assert_eq!(
            format_toml_string_array(&["bell\x07del\x7f".into()]),
            r#"["bell\u0007del\u007F"]"#
        );
        assert_eq!(
            format_toml_string_array(&["back\x08slash\\".into()]),
            r#"["back\bslash\\"]"#
        );
    }

    #[test]
    fn resolve_bin_passes_through_paths_with_separators() {
        assert_eq!(
            resolve_bin("/usr/local/bin/codex").as_deref(),
            Some("/usr/local/bin/codex")
        );
        assert_eq!(
            resolve_bin("./relative/bin").as_deref(),
            Some("./relative/bin")
        );
    }

    #[test]
    fn resolve_bin_returns_none_for_unknown_binary() {
        assert!(resolve_bin("definitely_not_a_real_binary_ahdgshfkjahsdfkh").is_none());
    }

    #[test]
    fn resolve_bin_finds_sh_in_standard_path() {
        // `sh` is guaranteed to exist on any Unix system the daemon runs on.
        let path = resolve_bin("sh").expect("sh should resolve");
        assert!(path.starts_with('/'), "expected absolute path, got {path}");
        assert!(path.ends_with("/sh") || path.ends_with("/sh\n"));
    }

    fn seed_gemini_fixture(
        tmp_root: &std::path::Path,
        project_name: &str,
        project_root: &str,
        session_id: &str,
        iso: &str,
        task_id: Option<&str>,
    ) {
        let proj_dir = tmp_root.join(project_name);
        let chats = proj_dir.join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(proj_dir.join(".project_root"), project_root).unwrap();
        let first8 = &session_id[..8];
        let path = chats.join(format!("session-{iso}-{first8}.json"));
        let message_text = task_id
            .map(|task| format!("[scope] task: {task}"))
            .unwrap_or_default();
        std::fs::write(
            &path,
            format!(
                "{{\n  \"sessionId\": \"{session_id}\",\n  \"messages\": [{{\"text\": {message_text:?}}}]\n}}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn resolve_gemini_session_finds_cwd_from_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        seed_gemini_fixture(
            tmp.path(),
            "daystrom-mk2",
            "/home/user/repos/daystrom-mk2",
            "13683fa2-df9a-44f3-a068-4520b4dbb55b",
            "2026-04-18T19-18",
            None,
        );
        let cwd = resolve_gemini_session_cwd_in(tmp.path(), "13683fa2-df9a-44f3-a068-4520b4dbb55b")
            .expect("should resolve");
        assert_eq!(
            cwd,
            std::path::PathBuf::from("/home/user/repos/daystrom-mk2")
        );
    }

    #[test]
    fn resolve_gemini_session_returns_none_for_unknown_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        seed_gemini_fixture(
            tmp.path(),
            "proj-a",
            "/repo/a",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "2026-04-18T10-00",
            None,
        );
        // Different UUID — silent fork territory on the real Gemini CLI;
        // here we want None so the caller refuses.
        assert!(
            resolve_gemini_session_cwd_in(tmp.path(), "bbbbbbbb-1111-2222-3333-444444444444",)
                .is_none()
        );
    }

    #[test]
    fn resolve_gemini_session_rejects_prefix_collision() {
        // Two files share the first-8 prefix but have different full UUIDs.
        // The returned cwd must be the one whose file body actually
        // contains the requested UUID — not a neighbor.
        let tmp = tempfile::tempdir().unwrap();
        seed_gemini_fixture(
            tmp.path(),
            "proj-a",
            "/repo/a",
            "13683fa2-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "2026-04-18T19-00",
            None,
        );
        seed_gemini_fixture(
            tmp.path(),
            "proj-b",
            "/repo/b",
            "13683fa2-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "2026-04-18T20-00",
            None,
        );
        let cwd = resolve_gemini_session_cwd_in(tmp.path(), "13683fa2-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
            .expect("should resolve");
        assert_eq!(cwd, std::path::PathBuf::from("/repo/b"));
    }

    #[test]
    fn resolve_gemini_session_rejects_short_id() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve_gemini_session_cwd_in(tmp.path(), "short").is_none());
    }

    #[test]
    fn discover_gemini_session_prefers_matching_project() {
        let tmp = tempfile::tempdir().unwrap();
        seed_gemini_fixture(
            tmp.path(),
            "proj-a",
            "/repo/a",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "2026-04-18T10-00",
            None,
        );
        seed_gemini_fixture(
            tmp.path(),
            "proj-b",
            "/repo/b",
            "bbbbbbbb-1111-2222-3333-444444444444",
            "2026-04-18T10-01",
            None,
        );

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let sid = discover_gemini_session_in(tmp.path(), now_ms, "/repo/b", None)
            .expect("should resolve");
        assert_eq!(sid, "bbbbbbbb-1111-2222-3333-444444444444");
    }

    #[test]
    fn discover_gemini_session_returns_none_when_no_match() {
        let tmp = tempfile::tempdir().unwrap();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(discover_gemini_session_in(tmp.path(), now_ms, "/repo/missing", None).is_none());
    }

    #[test]
    fn discover_gemini_session_prefers_matching_task_marker() {
        let tmp = tempfile::tempdir().unwrap();
        seed_gemini_fixture(
            tmp.path(),
            "proj",
            "/repo/x",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "2026-04-18T10-00",
            Some("task-older"),
        );
        seed_gemini_fixture(
            tmp.path(),
            "proj",
            "/repo/x",
            "bbbbbbbb-1111-2222-3333-444444444444",
            "2026-04-18T10-01",
            Some("task-newer"),
        );

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let sid = discover_gemini_session_in(tmp.path(), now_ms, "/repo/x", Some("task-older"))
            .expect("should resolve older task by marker");
        assert_eq!(sid, "aaaaaaaa-1111-2222-3333-444444444444");
    }

    #[test]
    fn discover_gemini_session_with_task_marker_refuses_unmatched_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        seed_gemini_fixture(
            tmp.path(),
            "proj",
            "/repo/x",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "2026-04-18T10-00",
            Some("some-other-task"),
        );

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(
            discover_gemini_session_in(tmp.path(), now_ms, "/repo/x", Some("missing-task"))
                .is_none(),
            "task-scoped discovery must wait for an exact marker match"
        );
    }

    fn seed_claude_fixture(
        home: &std::path::Path,
        account: &str,
        slug: &str,
        session_id: &str,
        cwd: &str,
    ) {
        let projects = home.join(account).join("projects").join(slug);
        std::fs::create_dir_all(&projects).unwrap();
        let path = projects.join(format!("{session_id}.jsonl"));
        let l1 = format!(r#"{{"type":"permission-mode","sessionId":"{session_id}"}}"#);
        let l2 = format!(r#"{{"type":"system","cwd":"{cwd}","sessionId":"{session_id}"}}"#);
        std::fs::write(&path, format!("{l1}\n{l2}\n")).unwrap();
    }

    #[test]
    fn resolve_claude_session_finds_cwd_primary_account() {
        let tmp = tempfile::tempdir().unwrap();
        seed_claude_fixture(
            tmp.path(),
            ".claude",
            "-home-user-repos-proj",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "/home/user/repos/proj",
        );
        let cwd = resolve_claude_session_cwd_in(tmp.path(), "aaaaaaaa-1111-2222-3333-444444444444")
            .expect("should resolve");
        assert_eq!(cwd, std::path::PathBuf::from("/home/user/repos/proj"));
    }

    #[test]
    fn resolve_claude_session_spans_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        // Primary account has an unrelated session; target lives in account2.
        seed_claude_fixture(
            tmp.path(),
            ".claude",
            "-home-user-repos-a",
            "11111111-0000-0000-0000-000000000000",
            "/home/user/repos/a",
        );
        seed_claude_fixture(
            tmp.path(),
            ".claude-account2",
            "-home-user-repos-b",
            "22222222-0000-0000-0000-000000000000",
            "/home/user/repos/b",
        );
        let cwd = resolve_claude_session_cwd_in(tmp.path(), "22222222-0000-0000-0000-000000000000")
            .expect("should resolve from secondary account");
        assert_eq!(cwd, std::path::PathBuf::from("/home/user/repos/b"));
    }

    #[test]
    fn resolve_claude_session_returns_none_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        seed_claude_fixture(
            tmp.path(),
            ".claude",
            "-home-user-repos-a",
            "aaaaaaaa-1111-2222-3333-444444444444",
            "/home/user/repos/a",
        );
        assert!(
            resolve_claude_session_cwd_in(tmp.path(), "bbbbbbbb-1111-2222-3333-444444444444",)
                .is_none()
        );
    }

    fn seed_codex_fixture(
        codex_root: &std::path::Path,
        date: (&str, &str, &str),
        iso_time: &str,
        session_id: &str,
        cwd: &str,
    ) {
        let (y, m, d) = date;
        let dir = codex_root.join("sessions").join(y).join(m).join(d);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-{iso_time}-{session_id}.jsonl"));
        let meta = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{session_id}","cwd":"{cwd}","originator":"codex_exec"}}}}"#
        );
        std::fs::write(&path, format!("{meta}\n")).unwrap();
    }

    #[test]
    fn resolve_codex_session_finds_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        seed_codex_fixture(
            tmp.path(),
            ("2026", "04", "17"),
            "2026-04-17T23-53-39",
            "019d9f26-e455-7da0-9e6c-460a5bbb223d",
            "/home/user/repos/daystrom-mk2",
        );
        let cwd = resolve_codex_session_cwd_in(tmp.path(), "019d9f26-e455-7da0-9e6c-460a5bbb223d")
            .expect("should resolve");
        assert_eq!(
            cwd,
            std::path::PathBuf::from("/home/user/repos/daystrom-mk2")
        );
    }

    #[test]
    fn resolve_codex_session_returns_none_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        seed_codex_fixture(
            tmp.path(),
            ("2026", "04", "17"),
            "2026-04-17T10-00-00",
            "019d9f26-e455-7da0-9e6c-460a5bbb223d",
            "/home/user/repos/a",
        );
        assert!(
            resolve_codex_session_cwd_in(tmp.path(), "00000000-0000-0000-0000-000000000000",)
                .is_none()
        );
    }

    #[test]
    fn resolve_session_cwd_dispatches_per_provider() {
        // Copilot + Vibe have no cwd-aware store and should return None.
        assert!(Provider::Copilot.resolve_session_cwd("any").is_none());
        assert!(Provider::Vibe.resolve_session_cwd("any").is_none());
    }
}
