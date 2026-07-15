use crate::config::ProviderConfig;
use crate::orchestration::brofile::{BrofileContext, CodeMode, ProviderDefaultsMode};

use std::path::PathBuf;

use super::Provider;

/// Resolve the harness/provider binary name from env. GLM/DeepSeek/MiniMax/
/// Brodex/VibeBh ride the custom harness instead of a vendor CLI; transport +
/// credentials are selected via env (see brofile::resolve_provider_env).
///
/// Harness providers run this binary as one child process per dispatch by
/// default. `BLACKBOX_HARNESS_EXECUTION_MODE=in-process` selects the temporary
/// rollback path, which parses the same argv through the linked harness crate.
/// The allocator also uses this resolved name for its availability gate.
fn bin_with_env(provider: Provider) -> String {
    match provider {
        Provider::Glm
        | Provider::Deepseek
        | Provider::Minimax
        | Provider::Brodex
        | Provider::VibeBh => {
            std::env::var("BRO_HARNESS_BIN").unwrap_or_else(|_| "bro-harness".into())
        }
        Provider::Workflow => "workflow".into(),
    }
}

/// Extra path entries prepended for spawned provider processes and their child
/// tools. Agents often follow rendered instructions to run operator-local
/// helpers like `rtk`; those live outside launchd/systemd's narrow PATH on
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
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if path.is_empty() { None } else { Some(path) }
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
    /// Service tier passed to harness providers as `--service-tier`. Brodex
    /// forwards `priority` to OpenAI Responses as Codex `/fast`; `default` is
    /// persisted in session state but dropped from the request body.
    pub service_tier: Option<String>,
    /// JSON schema for structured output. When set, emitted as
    /// `--output-schema <json>` to the harness, which registers a synthetic
    /// `final_result` terminal tool.
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
        _ => model.to_string(),
    }
}

/// Providers that dispatch through `bro-harness` have NO built-in default
/// model — unlike the `claude`/`codex` CLIs, the harness bails with
/// `no --model …` when none is passed. The allocator path fills a model, but
/// raw `bro_exec { provider }` (no tier/pin) does not, so a model-less harness
/// dispatch dies silently with exit 1 and zero events. Default to the catalog
/// `.default` model here, at the single arg-building chokepoint, so every
/// caller is covered. Returns None for CLI-backed providers, which have their
/// own defaults.
fn harness_default_model(provider: Provider) -> Option<String> {
    if !matches!(
        provider,
        Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh
    ) {
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
    ) -> Vec<String>;
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
    ) -> Vec<String>;
}

/// Serialize the payload for the `--dispatch-context` flag. The DTO is plain
/// strings/enums, so serialization cannot fail in practice.
fn dispatch_context_flag(args: &mut Vec<String>, ctx: Option<&bro_protocol::DispatchContext>) {
    if let Some(ctx) = ctx
        && let Ok(json) = serde_json::to_string(ctx)
    {
        args.extend(["--dispatch-context".into(), json]);
    }
}

impl ProviderExec for Provider {
    fn bin(&self) -> String {
        bin_with_env(*self)
    }

    fn bin_with_config(&self, _cfg: &ProviderConfig) -> String {
        match self {
            // Harness providers normally execute this child binary. The same
            // name also feeds the availability gate. No dedicated config
            // override exists today.
            Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh => bin_with_env(*self),
            Provider::Workflow => "workflow".into(),
        }
    }

    fn build_exec_args(
        &self,
        prompt: &str,
        dispatch_context: Option<&bro_protocol::DispatchContext>,
        session_id: &str,
        _cwd: Option<&str>,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts
            .and_then(|o| o.model.as_deref())
            .map(|m| normalize_model_for_provider(*self, m))
            .or_else(|| harness_default_model(*self));
        let effort = opts.and_then(|o| o.effort.as_deref());
        let code_mode = opts.and_then(|o| o.code_mode);
        let service_tier = opts.and_then(|o| o.service_tier.as_deref());
        let output_schema = opts.and_then(|o| o.output_schema.as_deref());
        let suppress_provider_defaults = opts
            .and_then(|o| o.provider_defaults)
            .is_some_and(ProviderDefaultsMode::suppresses);

        match self {
            Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh => {
                let mut args = vec![
                    "-p".into(),
                    prompt.into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--include-partial-messages".into(),
                    "--dangerously-skip-permissions".into(),
                ];
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
                if let Some(tier) = service_tier {
                    args.extend(["--service-tier".into(), tier.into()]);
                }
                if let Some(schema) = output_schema {
                    args.extend(["--output-schema".into(), schema.into()]);
                }
                args
            }
            Provider::Workflow => Vec::new(),
        }
    }

    fn build_resume_args(
        &self,
        session_id: &str,
        prompt: &str,
        dispatch_context: Option<&bro_protocol::DispatchContext>,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts
            .and_then(|o| o.model.as_deref())
            .map(|m| normalize_model_for_provider(*self, m))
            .or_else(|| harness_default_model(*self));
        let effort = opts.and_then(|o| o.effort.as_deref());
        let code_mode = opts.and_then(|o| o.code_mode);
        let service_tier = opts.and_then(|o| o.service_tier.as_deref());
        let output_schema = opts.and_then(|o| o.output_schema.as_deref());
        let suppress_provider_defaults = opts
            .and_then(|o| o.provider_defaults)
            .is_some_and(ProviderDefaultsMode::suppresses);

        match self {
            Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh => {
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
            Provider::Workflow => Vec::new(),
        }
    }
}
