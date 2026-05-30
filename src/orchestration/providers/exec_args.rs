use crate::config::ProviderConfig;
use crate::orchestration::brofile::{BrofileContext, ProviderDefaultsMode};

use super::Provider;
use super::mcp_args::{claude_mcp_config_json, transient_blackbox_name, transient_blackbox_url};

impl Provider {
    pub fn bin(&self) -> String {
        self.bin_with_env()
    }

    fn bin_with_env(&self) -> String {
        match self {
            Provider::Claude => std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
            // GLM/DeepSeek/Brodex ride the custom harness instead of a vendor
            // CLI; transport + credentials are selected via env (see
            // brofile::resolve_provider_env).
            Provider::Glm | Provider::Deepseek | Provider::Brodex => {
                std::env::var("BRO_HARNESS_BIN").unwrap_or_else(|_| "bro-harness".into())
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

    pub fn bin_with_config(&self, cfg: &ProviderConfig) -> String {
        match self {
            Provider::Claude => cfg
                .claude_bin
                .clone()
                .unwrap_or_else(|| self.bin_with_env()),
            // Harness providers resolve via BRO_HARNESS_BIN (bin_with_env);
            // no dedicated config override today.
            Provider::Glm | Provider::Deepseek | Provider::Brodex => self.bin_with_env(),
            Provider::Inception => cfg
                .opencode_bin
                .clone()
                .unwrap_or_else(|| self.bin_with_env()),
            Provider::Codex => cfg.codex_bin.clone().unwrap_or_else(|| self.bin_with_env()),
            Provider::Copilot => cfg
                .copilot_bin
                .clone()
                .unwrap_or_else(|| self.bin_with_env()),
            Provider::Vibe => cfg.vibe_bin.clone().unwrap_or_else(|| self.bin_with_env()),
            Provider::Gemini => cfg
                .gemini_bin
                .clone()
                .unwrap_or_else(|| self.bin_with_env()),
            Provider::Workflow => "workflow".into(),
        }
    }
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
    if path.is_empty() { None } else { Some(path) }
}

#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub provider_defaults: Option<ProviderDefaultsMode>,
}

const EMPTY_SYSTEM_PROMPT_OVERRIDE: &str = "";
pub(super) const CODEX_SUPPRESSED_INSTRUCTIONS_OVERRIDE: &str = concat!(
    "instructions=\"",
    "You are operating autonomously in non-interactive mode. ",
    "Do not use AskUserQuestion tools; halt if you encounter an unresolvable ambiguity.",
    "\""
);
const CODEX_EMPTY_DEVELOPER_INSTRUCTIONS_OVERRIDE: &str = "developer_instructions=\"\"";
pub(super) const CODEX_DISABLE_PROJECT_DOCS_OVERRIDE: &str = "project_doc_max_bytes=0";
pub(super) const CODEX_DISABLE_PERMISSIONS_INSTRUCTIONS_OVERRIDE: &str =
    "include_permissions_instructions=false";
const CODEX_DISABLE_APPS_INSTRUCTIONS_OVERRIDE: &str = "include_apps_instructions=false";
const CODEX_DISABLE_COLLABORATION_INSTRUCTIONS_OVERRIDE: &str =
    "include_collaboration_mode_instructions=false";
const CODEX_DISABLE_ENVIRONMENT_CONTEXT_OVERRIDE: &str = "include_environment_context=false";
pub(super) const CODEX_DISABLE_SKILL_INSTRUCTIONS_OVERRIDE: &str =
    "skills.include_instructions=false";

fn append_codex_suppression_config(args: &mut Vec<String>) {
    for override_value in [
        CODEX_SUPPRESSED_INSTRUCTIONS_OVERRIDE,
        CODEX_EMPTY_DEVELOPER_INSTRUCTIONS_OVERRIDE,
        CODEX_DISABLE_PROJECT_DOCS_OVERRIDE,
        CODEX_DISABLE_PERMISSIONS_INSTRUCTIONS_OVERRIDE,
        CODEX_DISABLE_APPS_INSTRUCTIONS_OVERRIDE,
        CODEX_DISABLE_COLLABORATION_INSTRUCTIONS_OVERRIDE,
        CODEX_DISABLE_ENVIRONMENT_CONTEXT_OVERRIDE,
        CODEX_DISABLE_SKILL_INSTRUCTIONS_OVERRIDE,
    ] {
        args.extend(["-c".into(), override_value.into()]);
    }
}

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
        Provider::Glm | Provider::Deepseek | Provider::Brodex
    ) {
        return None;
    }
    provider
        .models()
        .iter()
        .find(|m| m.default)
        .map(|m| m.id.to_string())
}

impl Provider {
    pub fn build_exec_args(
        &self,
        prompt: &str,
        session_id: &str,
        cwd: Option<&str>,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts
            .and_then(|o| o.model.as_deref())
            .map(|m| normalize_model_for_provider(*self, m))
            .or_else(|| harness_default_model(*self));
        let effort = opts.and_then(|o| o.effort.as_deref());
        let suppress_provider_defaults = opts
            .and_then(|o| o.provider_defaults)
            .is_some_and(ProviderDefaultsMode::suppresses);

        match self {
            Provider::Claude | Provider::Glm | Provider::Deepseek | Provider::Brodex => {
                let mut args = vec![
                    "-p".into(),
                    prompt.into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--include-partial-messages".into(),
                    "--dangerously-skip-permissions".into(),
                ];
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
                // Transient MCP inject ensures dispatched subprocesses see
                // blackbox regardless of which config file the bare CLI loads.
                if let Some(url) = transient_blackbox_url() {
                    let name = transient_blackbox_name();
                    args.extend(["--mcp-config".into(), claude_mcp_config_json(&name, &url)]);
                }
                args
            }
            Provider::Inception => {
                let mut args = vec![
                    "run".into(),
                    "--format".into(),
                    "json".into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model.as_deref() {
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
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
                }
                if suppress_provider_defaults {
                    append_codex_suppression_config(&mut args);
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
                if let Some(m) = model.as_deref() {
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
                // Vibe CLI has no `--model` flag; model selection is out-of-band.
                let _ = model;
                vec![
                    "-p".into(),
                    prompt.into(),
                    "--output".into(),
                    "json".into(),
                ]
            }
            Provider::Gemini => {
                let mut args = vec![
                    "-p".into(),
                    prompt.into(),
                    "--yolo".into(),
                    "--skip-trust".into(),
                    "-o".into(),
                    "json".into(),
                ];
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                args
            }
            Provider::Workflow => Vec::new(),
        }
    }

    pub fn build_resume_args(
        &self,
        session_id: &str,
        prompt: &str,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts
            .and_then(|o| o.model.as_deref())
            .map(|m| normalize_model_for_provider(*self, m))
            .or_else(|| harness_default_model(*self));
        let effort = opts.and_then(|o| o.effort.as_deref());
        let suppress_provider_defaults = opts
            .and_then(|o| o.provider_defaults)
            .is_some_and(ProviderDefaultsMode::suppresses);

        match self {
            Provider::Claude | Provider::Glm | Provider::Deepseek | Provider::Brodex => {
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
                if let Some(url) = transient_blackbox_url() {
                    let name = transient_blackbox_name();
                    args.extend(["--mcp-config".into(), claude_mcp_config_json(&name, &url)]);
                }
                args
            }
            Provider::Inception => {
                let mut args = vec![
                    "run".into(),
                    "--format".into(),
                    "json".into(),
                    "--session".into(),
                    session_id.into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model.as_deref() {
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
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
                }
                if suppress_provider_defaults {
                    append_codex_suppression_config(&mut args);
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
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                if let Some(e) = effort {
                    args.extend(["--effort".into(), e.into()]);
                }
                args
            }
            Provider::Vibe => {
                // Vibe CLI has no `--model` flag; see build_exec_args.
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
                    "--skip-trust".into(),
                    "-o".into(),
                    "json".into(),
                ];
                if let Some(m) = model.as_deref() {
                    args.extend(["--model".into(), m.into()]);
                }
                args
            }
            Provider::Workflow => Vec::new(),
        }
    }
}
