use crate::config::ProviderConfig;
use crate::orchestration::brofile::{
    BrofileContext, CodeMode, EditDiscipline, ProviderDefaultsMode,
};

use std::path::PathBuf;

use super::Provider;
use bro_core::ProviderLane;

/// Resolve the provider binary name from env. The claude lane runs the
/// vendor `claude` CLI (`CLAUDE_BIN` overrides); the codex lane runs the
/// `codex app-server` adapter (`CODEX_BIN` overrides); the harness lane runs the
/// standalone `bro-harness` binary (`BRO_HARNESS_BIN` overrides). Credentials
/// and endpoints are selected via env (see brofile::resolve_provider_env).
///
/// Dispatch composes a typed invocation and environment for the execution
/// host. Each lane owns its provider protocol and uses the daemon MCP endpoint.
fn bin_with_env(provider: Provider) -> String {
    match provider.lane() {
        ProviderLane::ClaudeCli => std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
        ProviderLane::Codex => std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into()),
        ProviderLane::Harness => {
            std::env::var("BRO_HARNESS_BIN").unwrap_or_else(|_| "bro-harness".into())
        }
        ProviderLane::Workflow => "workflow".into(),
    }
}

/// Extra path entries prepended for spawned provider processes and their child
/// tools. Agents often follow rendered instructions to run operator-local
/// helpers (cargo-installed or `~/.local/bin` tools); those live outside launchd/systemd's narrow PATH on
/// many hosts. Keep the fallback list small and user-local.
pub fn dispatch_extra_path_entries() -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if let Ok(raw) = std::env::var("BRO_EXTRA_PATH") {
        entries.extend(std::env::split_paths(&raw).filter(|path| !path.as_os_str().is_empty()));
    }
    if let Some(home) = dirs::home_dir() {
        entries.push(home.join(".local").join("bin"));
        entries.push(home.join(".cargo").join("bin"));
    }
    entries
}

pub fn dispatch_path_env() -> String {
    let mut entries = dispatch_extra_path_entries();
    if let Some(path) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(entries)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
        .to_string_lossy()
        .into_owned()
}

/// Resolve a provider binary name to an absolute path using a login shell.
///
/// The daemon is typically launched from `launchctl` / `systemd` with a
/// narrow, static `PATH` - it does not source `.bashrc`, `.zshrc`, `nvm.sh`,
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
    let augmented_path = dispatch_path_env();
    let output = std::process::Command::new("bash")
        .args(["-lc", &format!("command -v '{bin}'")])
        .env("PATH", &augmented_path)
        .output()
        .ok();
    if let Some(output) = output
        && output.status.success()
        && let Ok(stdout) = String::from_utf8(output.stdout)
    {
        let path = stdout.trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }
    // A Debian login shell's /etc/profile plainly reassigns PATH, clobbering
    // the augmented env above (macOS's path_helper preserves injected
    // entries), so a shell miss falls back to walking the augmented PATH
    // directly. Keeps rc-file resolution (nvm, asdf) first where it works.
    find_in_path_env(bin, &augmented_path)
}

/// Walk a PATH-style string for an executable named `bin`.
fn find_in_path_env(bin: &str, path_env: &str) -> Option<String> {
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        if let Some(path) = candidate.to_str() {
            return Some(path.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub provider_defaults: Option<ProviderDefaultsMode>,
    /// Code-mode to pass the harness as `--code-mode` (harness-backed providers
    /// only). `None` ⇒ no flag emitted, so the harness applies its own
    /// precedence (persisted session value → env → default `optional`).
    pub code_mode: Option<CodeMode>,
    /// Edit discipline to pass the harness as `--edit-discipline` on a
    /// fresh dispatch. Never passed on resume: the session saved it.
    pub edit_discipline: Option<EditDiscipline>,
    /// Service tier passed to harness providers as `--service-tier`. Brodex
    /// forwards `priority` to OpenAI Responses as Codex `/fast`; `default` is
    /// persisted in session state but dropped from the request body.
    pub service_tier: Option<String>,
    /// JSON schema for structured output. The claude lane passes it as
    /// `--json-schema <json>`; the harness lane as `--output-schema <json>`,
    /// which registers a synthetic `final_result` terminal tool.
    pub output_schema: Option<String>,
}

const EMPTY_SYSTEM_PROMPT_OVERRIDE: &str = "";

impl ExecOpts {
    pub fn with_provider_defaults(mut self, context: Option<&BrofileContext>) -> Self {
        if self.provider_defaults.is_none() {
            self.provider_defaults = context.and_then(|c| c.provider_defaults);
        }
        self
    }
}

pub fn exec_opts_with_provider_defaults(
    opts: Option<ExecOpts>,
    context: Option<&BrofileContext>,
) -> Option<ExecOpts> {
    let mode = context.and_then(|c| c.provider_defaults);
    if opts.is_none() && mode.is_none() {
        return None;
    }
    Some(opts.unwrap_or_default().with_provider_defaults(context))
}

fn normalize_model_for_provider(provider: Provider, model: &str) -> String {
    match provider {
        Provider::Glm => model
            .strip_prefix("zai-coding-plan/")
            .unwrap_or(model)
            .to_string(),
        Provider::Deepseek => model.strip_prefix("deepseek/").unwrap_or(model).to_string(),
        Provider::Minimax => model.strip_prefix("minimax/").unwrap_or(model).to_string(),
        // Kimi: the wire id is bare `k3` — the Kimi-for-Coding endpoint
        // rejects the Claude Code slot ids `k3[1m]` / `kimi-k3[1m]` with
        // "set model id as `k3`" (2026-07-19 wire probe); the 1M window rides
        // the context-1m beta header, not the id suffix. Map the slot ids so
        // a model pin copied out of ~/.claude-k/settings.json (which carries
        // `k3[1m]`) still dispatches.
        Provider::Kimi => match model {
            "k3[1m]" | "kimi-k3[1m]" => "k3".to_string(),
            _ => model.strip_prefix("kimi/").unwrap_or(model).to_string(),
        },
        _ => model.to_string(),
    }
}

/// The model a dispatch runs when the caller pins none. The harness has no
/// built-in default and bails with `no --model …`; the claude-compatible
/// endpoints need a wire id that their config dir may not pin. Both get the
/// catalog `.default` model here, at the single arg-building chokepoint, so
/// every caller is covered.
fn default_model(provider: Provider) -> Option<String> {
    // Codex's typed composer supplies its catalog default separately.
    if provider.lane() == ProviderLane::Codex {
        return None;
    }
    if !provider.is_dispatchable() {
        return None;
    }
    provider
        .models()
        .iter()
        .find(|m| m.default)
        .map(|m| m.id.to_string())
}

/// Argument construction for spawning/resuming a provider (daemon-side: reaches
/// brofile/config/MCP-injection types). Part of the provider dispatch surface —
/// see [`super::dispatch_prelude`].
pub trait ProviderExec {
    /// The binary the daemon spawns for this provider.
    fn bin(&self) -> String;
    /// Like [`bin`](Self::bin) but allowing a per-provider config override.
    fn bin_with_config(&self, cfg: &ProviderConfig) -> String;
    /// Build the args for a fresh dispatch. `prompt` is the operator's text
    /// VERBATIM; `dispatch_context` is the typed payload the harness composes
    /// per transport (dispatch-prompt-slots.md §4/§6) — the two never mix.
    fn build_exec_args(
        &self,
        prompt: &str,
        dispatch_context: Option<&bro_protocol::DispatchContext>,
        session_id: &str,
        cwd: Option<&str>,
        opts: Option<&ExecOpts>,
    ) -> ProviderLaunch;
    /// Build the args for resuming an existing session. Resume passes the
    /// full dispatch context too — persona included (the harness places it
    /// idempotently in the system slot; dropping it on resume was the old
    /// `_lens` bug, dispatch-prompt-slots.md §6).
    fn build_resume_args(
        &self,
        session_id: &str,
        prompt: &str,
        dispatch_context: Option<&bro_protocol::DispatchContext>,
        opts: Option<&ExecOpts>,
    ) -> ProviderLaunch;
}

/// Provider invocation before execution-host composition.
#[derive(Debug, Clone, Default)]
pub struct ProviderLaunch {
    pub argv: Vec<String>,
    pub codex: Option<bro_protocol::CodexSessionConfig>,
    pub prompt: Option<String>,
    pub errors: Vec<String>,
    /// A Claude allowlist requires a closed MCP inventory owned by this daemon.
    pub claude_restricted_mcp: bool,
}

impl From<Vec<String>> for ProviderLaunch {
    fn from(argv: Vec<String>) -> Self {
        Self {
            argv,
            ..Self::default()
        }
    }
}
impl std::ops::Deref for ProviderLaunch {
    type Target = Vec<String>;
    fn deref(&self) -> &Self::Target {
        &self.argv
    }
}
impl std::ops::DerefMut for ProviderLaunch {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.argv
    }
}
impl ProviderLaunch {
    pub fn apply_filters(&mut self, provider: Provider, filters: &super::super::mcp::McpFilters) {
        if let Some(config) = &mut self.codex {
            config.allowed_tools = super::mcp_args::expand_filter_patterns(&filters.allow);
            config.disallowed_tools = super::mcp_args::expand_filter_patterns(&filters.disallow);
        } else {
            use super::dispatch_prelude::ProviderMcp;
            if provider.lane() == ProviderLane::ClaudeCli && !filters.allow.is_empty() {
                match super::mcp_args::claude_allowlist_args(filters) {
                    Ok(args) => {
                        self.claude_restricted_mcp = true;
                        self.argv.extend(args);
                    }
                    Err(error) => self.errors.push(error.to_string()),
                }
                return;
            }
            self.argv.extend(provider.build_filter_args(filters));
        }
    }
}

fn cli_launch(provider: Provider, argv: Vec<String>, opts: Option<&ExecOpts>) -> ProviderLaunch {
    let mut launch = ProviderLaunch::from(argv);
    if provider.lane() == ProviderLane::ClaudeCli
        && let Some(opts) = opts
    {
        for (name, present) in [
            ("code_mode", opts.code_mode.is_some()),
            ("edit_discipline", opts.edit_discipline.is_some()),
            ("service_tier", opts.service_tier.is_some()),
        ] {
            if present {
                launch
                    .errors
                    .push(format!("Claude CLI does not support {name}"));
            }
        }
    }
    launch
}

fn codex_launch(
    prompt: &str,
    resume: Option<&str>,
    context: Option<&bro_protocol::DispatchContext>,
    opts: Option<&ExecOpts>,
) -> ProviderLaunch {
    let opts = opts.cloned().unwrap_or_default();
    let mut errors = Vec::new();
    if opts.code_mode.is_some() || opts.edit_discipline.is_some() {
        errors.push("Codex does not support harness code_mode or edit_discipline".into());
    }
    if opts
        .provider_defaults
        .is_some_and(ProviderDefaultsMode::suppresses)
    {
        errors.push("Codex does not support provider-default suppression".into());
    }
    let output_schema =
        opts.output_schema
            .as_deref()
            .and_then(|raw| match serde_json::from_str(raw) {
                Ok(value) => Some(value),
                Err(error) => {
                    errors.push(format!("invalid output schema: {error}"));
                    None
                }
            });
    ProviderLaunch {
        argv: Vec::new(),
        errors: Vec::new(),
        claude_restricted_mcp: false,
        prompt: Some(prompt.into()),
        codex: Some(bro_protocol::CodexSessionConfig {
            resume: resume.map(str::to_string),
            model: opts.model.or_else(|| {
                if resume.is_some() {
                    return None;
                }
                Provider::Codex
                    .models()
                    .iter()
                    .find(|model| model.default)
                    .map(|model| model.id.to_string())
            }),
            effort: opts.effort,
            service_tier: opts.service_tier,
            developer_instructions: context.and_then(render_dispatch_context_text),
            output_schema,
            errors,
            ..Default::default()
        }),
    }
}

/// Serialize the payload for the harness `--dispatch-context` flag. The DTO is
/// plain strings/enums, so serialization cannot fail in practice.
fn dispatch_context_flag(args: &mut Vec<String>, ctx: Option<&bro_protocol::DispatchContext>) {
    if let Some(ctx) = ctx
        && let Ok(json) = serde_json::to_string(ctx)
    {
        args.extend(["--dispatch-context".into(), json]);
    }
}

/// Render the dispatch context for the claude lane's `--append-system-prompt`:
/// the persona verbatim, then the scope ids as a fixed-order block. The claude
/// CLI has no typed slot for either, so the daemon composes the text; the
/// same text rides fresh dispatch and resume alike.
pub fn render_dispatch_context_text(ctx: &bro_protocol::DispatchContext) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(persona) = ctx.persona.as_deref()
        && !persona.trim().is_empty()
    {
        parts.push(persona.trim_end().to_string());
    }
    if let Some(scope) = ctx.scope.as_ref()
        && !scope.is_empty()
    {
        let mut block = String::from("## Dispatch scope\n");
        for (key, value) in scope.fields() {
            block.push_str(&format!("- {key}: {value}\n"));
        }
        parts.push(block.trim_end().to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn dispatch_context_append_flag(
    args: &mut Vec<String>,
    ctx: Option<&bro_protocol::DispatchContext>,
) {
    if let Some(text) = ctx.and_then(render_dispatch_context_text) {
        args.extend(["--append-system-prompt".into(), text]);
    }
}

/// The stream-json flags both lanes share: the harness deliberately mirrors
/// the claude CLI's headless surface.
fn stream_json_base_args(prompt: &str) -> Vec<String> {
    vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--dangerously-skip-permissions".into(),
    ]
}

impl ProviderExec for Provider {
    fn bin(&self) -> String {
        bin_with_env(*self)
    }

    fn bin_with_config(&self, _cfg: &ProviderConfig) -> String {
        // Each lane shares one binary selected by env. No dedicated
        // per-provider config override today.
        bin_with_env(*self)
    }

    fn build_exec_args(
        &self,
        prompt: &str,
        dispatch_context: Option<&bro_protocol::DispatchContext>,
        session_id: &str,
        _cwd: Option<&str>,
        opts: Option<&ExecOpts>,
    ) -> ProviderLaunch {
        if *self == Provider::Codex {
            return codex_launch(prompt, None, dispatch_context, opts);
        }
        let model = opts
            .and_then(|o| o.model.as_deref())
            .map(|m| normalize_model_for_provider(*self, m))
            .or_else(|| default_model(*self));
        let effort = opts.and_then(|o| o.effort.as_deref());
        let code_mode = opts.and_then(|o| o.code_mode);
        let edit_discipline = opts.and_then(|o| o.edit_discipline);
        let service_tier = opts.and_then(|o| o.service_tier.as_deref());
        let output_schema = opts.and_then(|o| o.output_schema.as_deref());
        let suppress_provider_defaults = opts
            .and_then(|o| o.provider_defaults)
            .is_some_and(ProviderDefaultsMode::suppresses);

        let argv = match self.lane() {
            ProviderLane::ClaudeCli => {
                let mut args = stream_json_base_args(prompt);
                dispatch_context_append_flag(&mut args, dispatch_context);
                if suppress_provider_defaults {
                    args.extend([
                        "--system-prompt".into(),
                        EMPTY_SYSTEM_PROMPT_OVERRIDE.into(),
                    ]);
                }
                if !session_id.is_empty() && session_id != "pending" {
                    args.extend(["--session-id".into(), session_id.into()]);
                }
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                if let Some(schema) = output_schema {
                    args.extend(["--json-schema".into(), schema.into()]);
                }
                args
            }
            ProviderLane::Harness => {
                let mut args = stream_json_base_args(prompt);
                dispatch_context_flag(&mut args, dispatch_context);
                if suppress_provider_defaults {
                    args.extend([
                        "--system-prompt".into(),
                        EMPTY_SYSTEM_PROMPT_OVERRIDE.into(),
                    ]);
                }
                if !session_id.is_empty() && session_id != "pending" {
                    args.extend(["--session-id".into(), session_id.into()]);
                }
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                if let Some(cm) = code_mode {
                    args.extend(["--code-mode".into(), cm.as_str().into()]);
                }
                if let Some(discipline) = edit_discipline {
                    args.extend(["--edit-discipline".into(), discipline.as_str().into()]);
                }
                if let Some(tier) = service_tier {
                    args.extend(["--service-tier".into(), tier.into()]);
                }
                if let Some(schema) = output_schema {
                    args.extend(["--output-schema".into(), schema.into()]);
                }
                args
            }
            ProviderLane::Codex => unreachable!("Codex uses typed launch settings"),
            ProviderLane::Workflow => Vec::new(),
        };
        cli_launch(*self, argv, opts)
    }

    fn build_resume_args(
        &self,
        session_id: &str,
        prompt: &str,
        dispatch_context: Option<&bro_protocol::DispatchContext>,
        opts: Option<&ExecOpts>,
    ) -> ProviderLaunch {
        if *self == Provider::Codex {
            return codex_launch(prompt, Some(session_id), dispatch_context, opts);
        }
        let model = opts
            .and_then(|o| o.model.as_deref())
            .map(|m| normalize_model_for_provider(*self, m))
            .or_else(|| default_model(*self));
        let effort = opts.and_then(|o| o.effort.as_deref());
        let code_mode = opts.and_then(|o| o.code_mode);
        let service_tier = opts.and_then(|o| o.service_tier.as_deref());
        let output_schema = opts.and_then(|o| o.output_schema.as_deref());
        let suppress_provider_defaults = opts
            .and_then(|o| o.provider_defaults)
            .is_some_and(ProviderDefaultsMode::suppresses);

        let argv = match self.lane() {
            ProviderLane::ClaudeCli => {
                let mut args = vec!["--resume".into(), session_id.into()];
                args.extend(stream_json_base_args(prompt));
                dispatch_context_append_flag(&mut args, dispatch_context);
                if suppress_provider_defaults {
                    args.extend([
                        "--system-prompt".into(),
                        EMPTY_SYSTEM_PROMPT_OVERRIDE.into(),
                    ]);
                }
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                if let Some(schema) = output_schema {
                    args.extend(["--json-schema".into(), schema.into()]);
                }
                args
            }
            ProviderLane::Harness => {
                let mut args = vec!["--resume".into(), session_id.into()];
                args.extend(stream_json_base_args(prompt));
                dispatch_context_flag(&mut args, dispatch_context);
                if suppress_provider_defaults {
                    args.extend([
                        "--system-prompt".into(),
                        EMPTY_SYSTEM_PROMPT_OVERRIDE.into(),
                    ]);
                }
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                // Resume normally leaves this None — the harness restores the
                // session's persisted code_mode. Emitted only if a caller
                // explicitly overrides it on resume.
                if let Some(cm) = code_mode {
                    args.extend(["--code-mode".into(), cm.as_str().into()]);
                }
                if let Some(tier) = service_tier {
                    args.extend(["--service-tier".into(), tier.into()]);
                }
                if let Some(schema) = output_schema {
                    args.extend(["--output-schema".into(), schema.into()]);
                }
                args
            }
            ProviderLane::Codex => unreachable!("Codex uses typed launch settings"),
            ProviderLane::Workflow => Vec::new(),
        };
        cli_launch(*self, argv, opts)
    }
}
