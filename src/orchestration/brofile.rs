use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::allocator::RuntimeRequest;
use super::mcp::McpFilters;
use super::providers::{Capability, Provider};

// ---------------------------------------------------------------------------
// Brofile
// ---------------------------------------------------------------------------

/// How dispatches using a brofile are hosted.
///
/// `Native` (default) is the existing headless dispatch: the provider CLI runs
/// as a child process whose stdout stream-json is the dispatch channel.
///
/// `Tmux` runs the provider's interactive TUI inside a tmux pane and resolves
/// the turn from the transcript read plane. Only valid for
/// `Provider::tui_capable()` providers (Claude/Codex). It is a brofile attribute
/// so every dispatch path — workflow executor actors, `bro_exec`, `bro_resume`
/// — picks it up from the resolved brofile without per-call flags. See
/// `design/orchestration/workflows/tmux-terminal-mode-slice.md`.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    #[default]
    Native,
    Tmux,
}

impl TerminalMode {
    pub fn is_native(&self) -> bool {
        matches!(self, TerminalMode::Native)
    }
    pub fn is_tmux(&self) -> bool {
        matches!(self, TerminalMode::Tmux)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brofile {
    pub name: String,
    pub provider: Provider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Persona-bound tool filter overlay. Merges between project mcp.json
    /// and per-dispatch ExecParams overrides at dispatch time. Lets a
    /// brofile (e.g. "auditor") restrict the tool surface every member
    /// inherits without touching global/project config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<McpFilters>,
    /// When true, inject the workspace-tools appendix into every dispatch
    /// using this brofile. The appendix teaches the agent to prefer
    /// workspace-scoped tools (work_smart_read, work_bash, work_git_*)
    /// over raw filesystem access, falling back to bbox_note(kind=learned)
    /// when workspace tools are unavailable. Default off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coerce_workspace: Option<bool>,
    /// Optional late-bound runtime allocation defaults. When present,
    /// dispatch resolves provider/account/model/effort through the
    /// allocator before falling through to the normal spawn path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeRequest>,
    /// Optional context-assembly policy. v1 carries only
    /// `provider_defaults`, the knob that suppresses provider-loaded
    /// harness markdown and default system prompts. Template and
    /// context-producer fields are not part of v1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<BrofileContext>,
    /// How dispatches using this brofile are hosted (headless vs tmux TUI).
    /// Picked up by every dispatch path; only valid for TUI-capable providers.
    #[serde(default, skip_serializing_if = "TerminalMode::is_native")]
    pub terminal_mode: TerminalMode,
}

impl Brofile {
    /// True iff dispatches using this brofile run the provider TUI in tmux.
    pub fn is_terminal(&self) -> bool {
        self.terminal_mode.is_tmux()
    }
}

// ---------------------------------------------------------------------------
// Context-assembly policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct BrofileContext {
    /// Suppression mode for provider-loaded harness markdown and
    /// default system-prompt material. See `ProviderDefaultsMode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_defaults: Option<ProviderDefaultsMode>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDefaultsMode {
    /// Let the provider load its normal global/user/project markdown
    /// and system prompt. Current behavior.
    #[default]
    Default,
    /// Suppress provider markdown / default prompt material when the
    /// provider has known controls; warn when unsupported.
    SuppressWhenSupported,
    /// Fail closed if suppression is requested but unsupported by
    /// the provider.
    StrictSuppress,
    /// Use only operator-supplied prompt material; fail closed where
    /// the provider cannot honor suppression.
    ExplicitOnly,
}

impl ProviderDefaultsMode {
    pub fn suppresses(self) -> bool {
        !matches!(self, ProviderDefaultsMode::Default)
    }
}

/// Whether `provider` can honor a non-`Default` `ProviderDefaultsMode`
/// in v1. Claude-compatible transports use `--system-prompt ""` so the
/// provider default system prompt / CLAUDE.md-derived prompt material is
/// replaced while transient MCP config is still injected. Codex overrides
/// base instructions, suppresses injected instruction blocks, caps project
/// docs at zero, and runs through a CODEX_HOME overlay that omits global
/// AGENTS files while preserving config/auth/MCP entries. OpenCode-backed
/// providers suppress generated `instructions` config entries. Other
/// providers are recorded but unsupported until per-provider wiring lands.
pub fn provider_supports_defaults_suppression(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Claude
            | Provider::Glm
            | Provider::Deepseek
            | Provider::Codex
            | Provider::Inception
    )
}

/// Reject dispatch when the brofile demands suppression the provider
/// cannot enforce. `SuppressWhenSupported` is best-effort and never
/// fails closed; `StrictSuppress` and `ExplicitOnly` do.
pub fn enforce_provider_defaults(
    provider: Provider,
    context: Option<&BrofileContext>,
) -> Result<(), String> {
    let mode = context
        .and_then(|c| c.provider_defaults)
        .unwrap_or_default();
    if !mode.suppresses() || provider_supports_defaults_suppression(provider) {
        return Ok(());
    }
    match mode {
        ProviderDefaultsMode::StrictSuppress | ProviderDefaultsMode::ExplicitOnly => Err(format!(
            "error.provider_defaults_unsupported: provider {} cannot enforce {:?}",
            provider.as_str(),
            mode
        )),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Account {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDefault {
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BroConfig {
    #[serde(default)]
    pub accounts: HashMap<String, Account>,
    #[serde(default)]
    pub provider_defaults: HashMap<Provider, ProviderDefault>,
}

// ---------------------------------------------------------------------------
// Disk operations
// ---------------------------------------------------------------------------

fn brofiles_dir(store_dir: &Path) -> PathBuf {
    store_dir.join("brofiles")
}

fn project_brofiles_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".bro").join("brofiles")
}

pub fn save_brofile(
    bf: &Brofile,
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> std::io::Result<PathBuf> {
    let dir = if scope == "project" {
        project_brofiles_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        brofiles_dir(store_dir)
    };
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", bf.name));
    let tmp = dir.join(format!("{}.json.tmp", bf.name));
    let data = serde_json::to_string_pretty(bf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, &file)?;
    Ok(file)
}

pub fn load_brofile(name: &str, dir: &Path) -> Option<Brofile> {
    let file = dir.join(format!("{name}.json"));
    let data = fs::read_to_string(&file).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn resolve_brofile(name: &str, store_dir: &Path, project_dir: Option<&str>) -> Option<Brofile> {
    // Project-local overrides global
    if let Some(pd) = project_dir {
        if let Some(bf) = load_brofile(name, &project_brofiles_dir(Path::new(pd))) {
            return Some(bf);
        }
    }
    load_brofile(name, &brofiles_dir(store_dir))
}

pub fn list_brofiles(scope: &str, store_dir: &Path, project_dir: Option<&str>) -> Vec<Brofile> {
    let dir = if scope == "project" {
        project_brofiles_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        brofiles_dir(store_dir)
    };
    let mut result = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(data) = fs::read_to_string(&path) {
                    if let Ok(bf) = serde_json::from_str::<Brofile>(&data) {
                        result.push(bf);
                    }
                }
            }
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

pub fn delete_brofile(
    name: &str,
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> bool {
    let dir = if scope == "project" {
        project_brofiles_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        brofiles_dir(store_dir)
    };
    let file = dir.join(format!("{name}.json"));
    fs::remove_file(&file).is_ok()
}

// ---------------------------------------------------------------------------
// Config / accounts
// ---------------------------------------------------------------------------

fn config_file(store_dir: &Path) -> PathBuf {
    store_dir.join("config.json")
}

pub fn load_config(store_dir: &Path) -> BroConfig {
    let file = config_file(store_dir);
    fs::read_to_string(&file)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

pub fn save_config(config: &BroConfig, store_dir: &Path) {
    let file = config_file(store_dir);
    let tmp = store_dir.join("config.json.tmp");
    let _ = fs::create_dir_all(store_dir);
    if let Ok(data) = serde_json::to_string_pretty(config) {
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(data.as_bytes());
            let _ = f.sync_all();
            let _ = fs::rename(&tmp, &file);
        }
    }
}

pub fn load_account(name: &str, store_dir: &Path) -> Option<Account> {
    let config = load_config(store_dir);
    config.accounts.get(name).cloned()
}

pub fn provider_default_account(provider: Provider, store_dir: &Path) -> Option<String> {
    let config = load_config(store_dir);
    config
        .provider_defaults
        .get(&provider)
        .map(|entry| entry.account.clone())
}

pub fn effective_account(
    provider: Provider,
    explicit_account: Option<&str>,
    store_dir: &Path,
) -> Option<String> {
    explicit_account
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| provider_default_account(provider, store_dir))
}

fn write_json_file(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(value) {
        let _ = fs::write(path, data);
    }
}

fn default_opencode_config_path(store_dir: &Path, provider: Provider) -> PathBuf {
    store_dir.join("generated").join(format!(
        "{}-opencode-{}.json",
        provider.as_str(),
        uuid::Uuid::new_v4().simple()
    ))
}

fn skipped_codex_home_entry(name: &std::ffi::OsStr) -> bool {
    name == "AGENTS.md" || name == "AGENTS.override.md"
}

#[cfg(unix)]
fn symlink_codex_home_entry(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(not(unix))]
fn symlink_codex_home_entry(source: &Path, link: &Path) -> std::io::Result<()> {
    if source.is_file() {
        fs::copy(source, link).map(|_| ())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Codex suppressed home overlays require symlink support for directories",
        ))
    }
}

fn prepare_codex_suppressed_home(base_home: &Path, store_dir: &Path) -> std::io::Result<PathBuf> {
    let overlay = store_dir.join("generated").join(format!(
        "codex-home-suppressed-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir_all(&overlay)?;

    let entries = match fs::read_dir(base_home) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(overlay),
        Err(err) => return Err(err),
    };

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if skipped_codex_home_entry(&name) {
            continue;
        }
        symlink_codex_home_entry(&entry.path(), &overlay.join(name))?;
    }

    Ok(overlay)
}

struct OpencodeProfile {
    default_model: &'static str,
    small_model: &'static str,
}

fn opencode_profile(provider: Provider) -> Option<OpencodeProfile> {
    match provider {
        Provider::Inception => Some(OpencodeProfile {
            default_model: "inception/mercury-2",
            small_model: "inception/mercury-2",
        }),
        _ => None,
    }
}

fn build_opencode_config(
    provider: Provider,
    model: Option<&str>,
    suppress_instructions: bool,
) -> Value {
    let profile = opencode_profile(provider).expect("provider must be OpenCode-backed");
    let model = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(profile.default_model);
    let mut config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model,
        "small_model": profile.small_model,
        "tools": {
            "blackbox_bro_*": false
        }
    });

    // OpenCode does NOT follow Claude Code's `@import` syntax in
    // AGENTS.md/CLAUDE.md — those references stay as plain text. Wire
    // BLACKBOX.md (the provider-neutral global memory file) explicitly
    // via the `instructions` config field, which opencode reads, fetches,
    // and merges into the system prompt at the `Instructions from: <path>`
    // header. Existing files are added to the `instructions` array; missing
    // files are silently skipped by opencode (`fs.glob` returns `[]`).
    // When the caller's brofile asks for harness-defaults suppression,
    // omit the array entirely so opencode loads no blackbox-controlled
    // instructions.
    if !suppress_instructions {
        if let Some(home) = dirs::home_dir() {
            let blackbox_md = crate::util::blackbox_global_common_md_path(&home);
            if blackbox_md.exists() {
                config["instructions"] =
                    serde_json::json!([blackbox_md.to_string_lossy().into_owned()]);
            }
        }
    }

    if let Some(url) = super::providers::transient_blackbox_url() {
        config["mcp"] = serde_json::json!({
            super::providers::transient_blackbox_name(): {
                "type": "remote",
                "url": url,
                "enabled": true
            }
        });
    }

    config
}

fn default_opencode_env(
    provider: Provider,
    store_dir: &Path,
    model: Option<&str>,
    suppress_instructions: bool,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let config_path = default_opencode_config_path(store_dir, provider);
    write_json_file(
        &config_path,
        &build_opencode_config(provider, model, suppress_instructions),
    );
    env.insert(
        "OPENCODE_CONFIG".into(),
        config_path.to_string_lossy().into_owned(),
    );
    env
}

/// Harness env for the Anthropic-transport providers (GLM, DeepSeek). These
/// no longer run the `claude` CLI; they run `bro-harness`, which reads
/// `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` from its own env. We lift
/// those out of the operator's existing `~/.claude-{zai,ds}/settings.json`
/// `env` block (the same credentials the CLI used) and select the transport.
fn default_claude_compatible_env(
    provider: Provider,
    home_dir: &Path,
) -> Option<HashMap<String, String>> {
    let rel_path = match provider {
        Provider::Glm => ".claude-zai",
        Provider::Deepseek => ".claude-ds",
        _ => return None,
    };
    let mut env = HashMap::from([("BRO_HARNESS_TRANSPORT".to_string(), "anthropic".to_string())]);

    let settings = home_dir.join(rel_path).join("settings.json");
    if let Ok(body) = std::fs::read_to_string(&settings)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
    {
        for key in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ] {
            if let Some(val) = v["env"][key].as_str() {
                env.insert(key.to_string(), val.to_string());
            }
        }
    }
    Some(env)
}

/// Harness env for the vibe-bh provider — Mistral on the OpenAI
/// **chat-completions** transport. The base URL is fixed to the Mistral API and
/// the reasoning profile selects the Mistral effort/thinking shaping; the key is
/// lifted from `MISTRAL_API_KEY` in the process env, falling back to the
/// operator's `~/.vibe/.env` (where the vibe CLI stores it). The chat-transport
/// analogue of `default_claude_compatible_env`, and the credential-wiring
/// template for future OpenAI-compatible chat endpoints.
fn default_vibe_harness_env(home_dir: &Path) -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            "BRO_HARNESS_TRANSPORT".to_string(),
            "openai-chat".to_string(),
        ),
        (
            "BRO_HARNESS_CHAT_REASONING".to_string(),
            "mistral".to_string(),
        ),
        (
            "OPENAI_BASE_URL".to_string(),
            "https://api.mistral.ai/v1".to_string(),
        ),
    ]);

    if let Some(key) = std::env::var("MISTRAL_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .or_else(|| read_vibe_env_key(home_dir, "MISTRAL_API_KEY"))
    {
        env.insert("OPENAI_API_KEY".to_string(), key);
    }
    env
}

/// Read a single `KEY=value` entry from `~/.vibe/.env` (the vibe CLI's dotenv).
/// Tolerant of an `export ` prefix and surrounding quotes; returns None if the
/// file or key is absent.
fn read_vibe_env_key(home_dir: &Path, key: &str) -> Option<String> {
    let body = std::fs::read_to_string(home_dir.join(".vibe").join(".env")).ok()?;
    for raw in body.lines() {
        let line = raw.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some(rest) = line.strip_prefix(key)
            && let Some(val) = rest.trim_start().strip_prefix('=')
        {
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn normalized_account_suffix(name: &str) -> Option<String> {
    let lowered = name.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return None;
    }

    match lowered.as_str() {
        "default" | "primary" | "main" | "account1" | "yolo1" => return Some(String::new()),
        "account2" | "yolo2" => return Some("-account2".into()),
        "account3" | "yolo3" => return Some("-account3".into()),
        _ => {}
    }

    if let Some(rest) = lowered.strip_prefix("account") {
        return match rest {
            "" => None,
            "1" => Some(String::new()),
            _ => Some(format!("-account{rest}")),
        };
    }

    if let Some(rest) = lowered.strip_prefix("yolo") {
        return match rest {
            "" => None,
            "1" => Some(String::new()),
            _ => Some(format!("-account{rest}")),
        };
    }

    None
}

fn synthesized_account_env_for_home(
    provider: Provider,
    account_name: &str,
    home_dir: &Path,
) -> Option<HashMap<String, String>> {
    let suffix = normalized_account_suffix(account_name)?;

    let (env_key, rel_path) = match provider {
        Provider::Claude => ("CLAUDE_CONFIG_DIR", format!(".claude{suffix}")),
        // Codex and Brodex both ride the Codex/ChatGPT config dir.
        Provider::Codex | Provider::Brodex => ("CODEX_HOME", format!(".codex{suffix}")),
        // `gh` respects GH_CONFIG_DIR; keep the same account suffix pattern.
        Provider::Copilot => ("GH_CONFIG_DIR", format!(".config/gh{suffix}")),
        Provider::Glm
        | Provider::Deepseek
        | Provider::Inception
        | Provider::Gemini
        | Provider::Vibe
        // vibe-bh authenticates via MISTRAL_API_KEY (env / ~/.vibe/.env), not a
        // per-account config dir.
        | Provider::VibeBh
        | Provider::Workflow => return None,
    };

    Some(HashMap::from([(
        env_key.to_string(),
        home_dir.join(rel_path).to_string_lossy().into_owned(),
    )]))
}

pub fn resolve_provider_env(
    provider: Provider,
    account_name: Option<&str>,
    model: Option<&str>,
    store_dir: &Path,
    brofile_context: Option<&BrofileContext>,
) -> Option<HashMap<String, String>> {
    resolve_provider_env_inner(provider, account_name, model, store_dir, brofile_context)
}

fn resolve_provider_env_inner(
    provider: Provider,
    account_name: Option<&str>,
    model: Option<&str>,
    store_dir: &Path,
    brofile_context: Option<&BrofileContext>,
) -> Option<HashMap<String, String>> {
    let account_name = effective_account(provider, account_name, store_dir);
    let suppress_instructions = brofile_context
        .and_then(|c| c.provider_defaults)
        .map(ProviderDefaultsMode::suppresses)
        .unwrap_or(false);
    let mut env = match provider {
        Provider::Glm | Provider::Deepseek => dirs::home_dir()
            .as_deref()
            .and_then(|home| default_claude_compatible_env(provider, home))
            .unwrap_or_default(),
        // Brodex rides the harness on the OpenAI Responses transport against
        // the Codex/ChatGPT backend; CODEX_HOME (for OAuth) is supplied by the
        // account-env synthesis below, defaulting to ~/.codex in the harness.
        Provider::Brodex => HashMap::from([(
            "BRO_HARNESS_TRANSPORT".to_string(),
            "openai-responses".to_string(),
        )]),
        // vibe-bh rides the harness on the OpenAI chat-completions transport
        // against the Mistral API; base URL + reasoning profile are fixed and
        // the key comes from MISTRAL_API_KEY (env / ~/.vibe/.env).
        Provider::VibeBh => dirs::home_dir()
            .as_deref()
            .map(default_vibe_harness_env)
            .unwrap_or_default(),
        Provider::Inception => {
            default_opencode_env(provider, store_dir, model, suppress_instructions)
        }
        _ => HashMap::new(),
    };

    if let Some(account_name) = account_name.as_deref() {
        if !matches!(
            provider,
            Provider::Glm | Provider::Deepseek | Provider::Inception
        ) {
            if let Some(account_env) = dirs::home_dir()
                .as_deref()
                .and_then(|home| synthesized_account_env_for_home(provider, account_name, home))
            {
                env.extend(account_env);
            }
        }
    }

    if let Some(account_name) = account_name.as_deref() {
        if let Some(overrides) = load_account(account_name, store_dir).and_then(|a| a.env) {
            env.extend(overrides);
        }
    }

    if provider == Provider::Codex && suppress_instructions {
        let base_home = env
            .get("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".codex")));
        if let Some(base_home) = base_home {
            match prepare_codex_suppressed_home(&base_home, store_dir) {
                Ok(overlay) => {
                    env.insert(
                        "CODEX_HOME".to_string(),
                        overlay.to_string_lossy().into_owned(),
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        base_home = %base_home.display(),
                        error = %err,
                        "failed to prepare Codex suppressed home overlay"
                    );
                }
            }
        }
    }

    (!env.is_empty()).then_some(env)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_store() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn with_fake_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::util::test_env_lock();
        let prior = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home);
        }
        let result = f();
        match prior {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        result
    }

    #[test]
    fn test_save_and_load_brofile() {
        let dir = temp_store();
        let bf = Brofile {
            name: "reviewer".into(),
            provider: Provider::Claude,
            account: None,
            lens: Some("You are a code reviewer".into()),
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("reviewer", dir.path(), None);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.name, "reviewer");
        assert_eq!(loaded.provider, Provider::Claude);
        assert_eq!(loaded.lens.as_deref(), Some("You are a code reviewer"));
    }

    #[test]
    fn g11_save_brofile_returns_written_path_resolves_immediately() {
        // G11: artifact_install for kind=brofile silently desynced from
        // the runtime registry. Verify save_brofile now returns the on-disk
        // path AND that resolve_brofile sees the entry immediately, so
        // post-install verification at the install boundary cannot pass
        // while the runtime registry is still empty.
        let dir = temp_store();
        let bf = Brofile {
            name: "post-install-verification".into(),
            provider: Provider::Codex,
            account: None,
            lens: Some("test".into()),
            model: Some("gpt-5.5".into()),
            effort: Some("medium".into()),
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        let written = save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        assert!(
            written.exists(),
            "save_brofile must report the on-disk path"
        );
        assert!(
            resolve_brofile("post-install-verification", dir.path(), None).is_some(),
            "saved brofile must resolve immediately — G11 desync regression"
        );
    }

    #[test]
    fn test_project_scope_overrides_global() {
        let store = temp_store();
        let project = temp_store();

        let global_bf = Brofile {
            name: "worker".into(),
            provider: Provider::Claude,
            account: None,
            lens: Some("global lens".into()),
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&global_bf, "global", store.path(), None).expect("brofile save");

        let project_bf = Brofile {
            name: "worker".into(),
            provider: Provider::Codex,
            account: None,
            lens: Some("project lens".into()),
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(
            &project_bf,
            "project",
            store.path(),
            Some(project.path().to_str().unwrap()),
        )
        .expect("brofile save");

        let resolved = resolve_brofile(
            "worker",
            store.path(),
            Some(project.path().to_str().unwrap()),
        );
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().provider, Provider::Codex);
    }

    #[test]
    fn test_list_brofiles() {
        let dir = temp_store();
        for name in &["alpha", "beta", "gamma"] {
            let bf = Brofile {
                name: name.to_string(),
                provider: Provider::Claude,
                account: None,
                lens: None,
                model: None,
                effort: None,
                filters: None,
                coerce_workspace: None,
                runtime: None,
                context: None,
                terminal_mode: TerminalMode::Native,
            };
            save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        }
        let list = list_brofiles("global", dir.path(), None);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[2].name, "gamma");
    }

    #[test]
    fn test_delete_brofile() {
        let dir = temp_store();
        let bf = Brofile {
            name: "to_delete".into(),
            provider: Provider::Gemini,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        assert!(resolve_brofile("to_delete", dir.path(), None).is_some());
        assert!(delete_brofile("to_delete", "global", dir.path(), None));
        assert!(resolve_brofile("to_delete", dir.path(), None).is_none());
    }

    #[test]
    fn test_config_accounts() {
        let dir = temp_store();
        let mut config = load_config(dir.path());
        config.accounts.insert(
            "work".into(),
            Account {
                env: Some(HashMap::from([(
                    "CLAUDE_HOME".into(),
                    "/home/user/.claude-work".into(),
                )])),
                ..Default::default()
            },
        );
        save_config(&config, dir.path());

        let loaded = load_config(dir.path());
        assert!(loaded.accounts.contains_key("work"));
        let acct = &loaded.accounts["work"];
        assert_eq!(
            acct.env.as_ref().unwrap().get("CLAUDE_HOME").unwrap(),
            "/home/user/.claude-work"
        );
    }

    #[test]
    fn test_brofile_persists_filters() {
        let dir = temp_store();
        let bf = Brofile {
            name: "auditor".into(),
            provider: Provider::Claude,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: Some(McpFilters {
                allow: vec![],
                disallow: vec!["mcp__blackbox__bro_*".into(), "Bash(*)".into()],
            }),
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("auditor", dir.path(), None).unwrap();
        let f = loaded.filters.expect("filters round-trip");
        assert_eq!(f.disallow.len(), 2);
        assert!(f.disallow.contains(&"Bash(*)".to_string()));
        assert!(f.allow.is_empty());
    }

    #[test]
    fn rust_refactor_persona_matches_design_spec() {
        let src =
            include_str!("../../system-defaults/brofiles/refactor/rust-refactor-persona.json");
        let bf: Brofile = serde_json::from_str(src).expect("rust-refactor-persona parses");
        assert_eq!(bf.name, "rust-refactor-persona");
        assert_eq!(bf.provider, Provider::Claude);
        assert_eq!(
            bf.context.as_ref().and_then(|c| c.provider_defaults),
            Some(ProviderDefaultsMode::SuppressWhenSupported)
        );
        assert!(
            bf.lens
                .as_deref()
                .unwrap_or("")
                .contains("bbox refactor primitives")
        );

        let f = bf.filters.expect("filters present");

        let expected_allow: Vec<&str> = vec![
            "mcp__blackbox__bbox_code_symbols",
            "mcp__blackbox__bbox_code_node_describe",
            "mcp__blackbox__bbox_code_query",
            "mcp__blackbox__bbox_code_refs",
            "mcp__blackbox__bbox_refactor_status",
            "mcp__blackbox__bbox_refactor_project_refs",
            "mcp__blackbox__bbox_refactor_plan",
            "mcp__blackbox__bbox_refactor_apply",
            "mcp__blackbox__bbox_refactor_run",
            "mcp__blackbox__bbox_note",
            "mcp__blackbox__bbox_thread",
            "mcp__blackbox__bbox_pin",
            "mcp__blackbox__bbox_inspect_entity",
            "mcp__blackbox__bbox_hybrid_search",
            "Read",
            "Grep",
            "Glob",
        ];
        let expected_disallow: Vec<&str> = vec![
            "mcp__blackbox__bbox_forget",
            "mcp__blackbox__bbox_decide",
            "mcp__blackbox__bbox_learn",
            "mcp__blackbox__bbox_remember",
            "mcp__blackbox__bbox_render",
            "mcp__blackbox__bro_*",
            "Bash",
            "Write",
            "Edit",
        ];

        let allow_set: std::collections::BTreeSet<&str> =
            f.allow.iter().map(String::as_str).collect();
        let expected_allow_set: std::collections::BTreeSet<&str> =
            expected_allow.iter().copied().collect();
        assert_eq!(
            allow_set, expected_allow_set,
            "rust-refactor-persona allow list drifted from design spec"
        );

        let disallow_set: std::collections::BTreeSet<&str> =
            f.disallow.iter().map(String::as_str).collect();
        let expected_disallow_set: std::collections::BTreeSet<&str> =
            expected_disallow.iter().copied().collect();
        assert_eq!(
            disallow_set, expected_disallow_set,
            "rust-refactor-persona disallow list drifted from design spec"
        );
    }

    #[test]
    fn java_refactor_persona_matches_design_spec() {
        let src =
            include_str!("../../system-defaults/brofiles/refactor/java-refactor-persona.json");
        let bf: Brofile = serde_json::from_str(src).expect("java-refactor-persona parses");
        assert_eq!(bf.name, "java-refactor-persona");
        assert_eq!(bf.provider, Provider::Codex);
        assert_eq!(bf.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(bf.effort.as_deref(), Some("medium"));
        assert_eq!(
            bf.context.as_ref().and_then(|c| c.provider_defaults),
            Some(ProviderDefaultsMode::SuppressWhenSupported)
        );
        let lens = bf.lens.as_deref().unwrap_or("");
        assert!(lens.contains("bbox refactor primitives"));
        assert!(
            lens.contains("Lombok") || lens.contains("annotation"),
            "Java lens should reference Java-specific caveats (Lombok / annotation processors)"
        );

        let f = bf.filters.expect("filters present");

        let expected_allow: Vec<&str> = vec![
            "mcp__blackbox__bbox_code_symbols",
            "mcp__blackbox__bbox_code_node_describe",
            "mcp__blackbox__bbox_code_query",
            "mcp__blackbox__bbox_code_refs",
            "mcp__blackbox__bbox_refactor_status",
            "mcp__blackbox__bbox_refactor_project_refs",
            "mcp__blackbox__bbox_refactor_plan",
            "mcp__blackbox__bbox_refactor_apply",
            "mcp__blackbox__bbox_refactor_run",
            "mcp__blackbox__bbox_note",
            "mcp__blackbox__bbox_thread",
            "mcp__blackbox__bbox_pin",
            "mcp__blackbox__bbox_inspect_entity",
            "mcp__blackbox__bbox_hybrid_search",
            "Read",
            "Grep",
            "Glob",
        ];
        let expected_disallow: Vec<&str> = vec![
            "mcp__blackbox__bbox_forget",
            "mcp__blackbox__bbox_decide",
            "mcp__blackbox__bbox_learn",
            "mcp__blackbox__bbox_remember",
            "mcp__blackbox__bbox_render",
            "mcp__blackbox__bro_*",
            "Bash",
            "Write",
            "Edit",
        ];

        let allow_set: std::collections::BTreeSet<&str> =
            f.allow.iter().map(String::as_str).collect();
        let expected_allow_set: std::collections::BTreeSet<&str> =
            expected_allow.iter().copied().collect();
        assert_eq!(
            allow_set, expected_allow_set,
            "java-refactor-persona allow list drifted from design spec"
        );

        let disallow_set: std::collections::BTreeSet<&str> =
            f.disallow.iter().map(String::as_str).collect();
        let expected_disallow_set: std::collections::BTreeSet<&str> =
            expected_disallow.iter().copied().collect();
        assert_eq!(
            disallow_set, expected_disallow_set,
            "java-refactor-persona disallow list drifted from design spec"
        );
    }

    #[test]
    fn terminal_mode_defaults_native_and_parses_tmux() {
        // Absent -> Native (back-compat for every existing brofile).
        let bf: Brofile = serde_json::from_str(
            r#"{"name":"x","provider":"codex"}"#,
        )
        .unwrap();
        assert_eq!(bf.terminal_mode, TerminalMode::Native);
        assert!(!bf.is_terminal());

        // Explicit tmux.
        let bf: Brofile = serde_json::from_str(
            r#"{"name":"x","provider":"codex","terminal_mode":"tmux"}"#,
        )
        .unwrap();
        assert_eq!(bf.terminal_mode, TerminalMode::Tmux);
        assert!(bf.is_terminal());

        // Native is skipped on serialize (keeps existing brofile JSON stable).
        let json = serde_json::to_string(&Brofile {
            name: "x".into(),
            provider: Provider::Codex,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        })
        .unwrap();
        assert!(!json.contains("terminal_mode"), "{json}");

        // Unknown value rejected.
        assert!(
            serde_json::from_str::<Brofile>(
                r#"{"name":"x","provider":"codex","terminal_mode":"screen"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn phase_decomposer_edit_implementer_parses() {
        let src =
            include_str!("../../system-defaults/brofiles/phase-decompose/edit-implementer.json");
        let bf: Brofile = serde_json::from_str(src).expect("phase-decomposer edit brofile parses");
        assert_eq!(bf.name, "phase-decomposer-edit-implementer");
        assert_eq!(bf.provider, Provider::Codex);
        // Retiered to runtime allocation (commit 6ae3437): model/effort are no
        // longer hardcoded; the brofile selects a "premium" runtime tier.
        assert_eq!(bf.model, None);
        assert_eq!(bf.effort, None);
        assert_eq!(
            bf.runtime.as_ref().and_then(|r| r.tier.as_deref()),
            Some("premium")
        );
        assert_eq!(
            bf.context.as_ref().and_then(|c| c.provider_defaults),
            Some(ProviderDefaultsMode::SuppressWhenSupported)
        );
        let lens = bf.lens.as_deref().unwrap_or("");
        assert!(lens.contains("edit-capable bounded implementer"));
        assert!(lens.contains("must not dispatch or resume agents"));
        assert!(lens.contains("strict JSON only"));
    }

    /// Rust + Java refactor personas should expose the same allow/disallow
    /// sets — the refactor + grounding tool surface is language-agnostic at
    /// the MCP layer. Only the lens prose differs. The cross-language symmetry
    /// is load-bearing for refactor-atom interchange between brofiles.
    #[test]
    fn rust_and_java_refactor_personas_share_tool_surface() {
        let rust_src =
            include_str!("../../system-defaults/brofiles/refactor/rust-refactor-persona.json");
        let java_src =
            include_str!("../../system-defaults/brofiles/refactor/java-refactor-persona.json");
        let rust: Brofile = serde_json::from_str(rust_src).unwrap();
        let java: Brofile = serde_json::from_str(java_src).unwrap();

        let r = rust.filters.unwrap();
        let j = java.filters.unwrap();
        let r_allow: std::collections::BTreeSet<&str> =
            r.allow.iter().map(String::as_str).collect();
        let j_allow: std::collections::BTreeSet<&str> =
            j.allow.iter().map(String::as_str).collect();
        let r_disallow: std::collections::BTreeSet<&str> =
            r.disallow.iter().map(String::as_str).collect();
        let j_disallow: std::collections::BTreeSet<&str> =
            j.disallow.iter().map(String::as_str).collect();
        assert_eq!(
            r_allow, j_allow,
            "refactor personas should expose identical allow sets"
        );
        assert_eq!(
            r_disallow, j_disallow,
            "refactor personas should expose identical disallow sets"
        );
        assert_eq!(
            rust.context.as_ref().and_then(|c| c.provider_defaults),
            java.context.as_ref().and_then(|c| c.provider_defaults),
            "refactor personas should share context policy"
        );
    }

    #[test]
    fn refactor_atom_brofiles_suppress_provider_defaults() {
        for (name, src) in [
            (
                "rust-refactor-persona",
                include_str!("../../system-defaults/brofiles/refactor/rust-refactor-persona.json"),
            ),
            (
                "java-refactor-persona",
                include_str!("../../system-defaults/brofiles/refactor/java-refactor-persona.json"),
            ),
            (
                "csharp-refactor-persona",
                include_str!(
                    "../../system-defaults/brofiles/refactor/csharp-refactor-persona.json"
                ),
            ),
            (
                "elixir-refactor-persona",
                include_str!(
                    "../../system-defaults/brofiles/refactor/elixir-refactor-persona.json"
                ),
            ),
        ] {
            let bf: Brofile = serde_json::from_str(src)
                .unwrap_or_else(|e| panic!("{name} brofile should parse: {e}"));
            assert_eq!(
                bf.context.as_ref().and_then(|c| c.provider_defaults),
                Some(ProviderDefaultsMode::SuppressWhenSupported),
                "{name} should suppress provider defaults for profile-backed atoms"
            );
        }
    }

    #[test]
    fn phase_decomposer_scout_brofile_suppresses_defaults() {
        let src =
            include_str!("../../system-defaults/brofiles/phase-decompose/corpus-pathfinder.json");
        let bf: Brofile = serde_json::from_str(src).expect("corpus-pathfinder brofile parses");
        assert_eq!(bf.name, "corpus-pathfinder");
        assert_eq!(bf.provider, Provider::Codex);
        assert_eq!(
            bf.context.as_ref().and_then(|c| c.provider_defaults),
            Some(ProviderDefaultsMode::SuppressWhenSupported)
        );
        assert!(
            bf.lens
                .as_deref()
                .unwrap_or("")
                .contains("one focused discovery charter")
        );
    }

    #[test]
    fn test_brofile_with_model_effort() {
        let dir = temp_store();
        let bf = Brofile {
            name: "fast".into(),
            provider: Provider::Codex,
            account: None,
            lens: None,
            model: Some("gpt-5.4-mini".into()),
            effort: Some("low".into()),
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("fast", dir.path(), None).unwrap();
        assert_eq!(loaded.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(loaded.effort.as_deref(), Some("low"));
    }

    #[test]
    fn test_brofile_coerce_workspace_round_trip() {
        let dir = temp_store();
        let bf = Brofile {
            name: "ws-worker".into(),
            provider: Provider::Claude,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: Some(true),
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("ws-worker", dir.path(), None).unwrap();
        assert_eq!(loaded.coerce_workspace, Some(true));

        let bf_off = Brofile {
            name: "ws-off".into(),
            provider: Provider::Claude,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            terminal_mode: TerminalMode::Native,
        };
        save_brofile(&bf_off, "global", dir.path(), None).expect("brofile save");
        let loaded_off = resolve_brofile("ws-off", dir.path(), None).unwrap();
        assert_eq!(loaded_off.coerce_workspace, None);
    }

    #[test]
    fn test_brofile_coerce_workspace_deserializes_from_json() {
        let json = r#"{
            "name": "ws-json",
            "provider": "claude",
            "coerce_workspace": true
        }"#;
        let bf: Brofile = serde_json::from_str(json).unwrap();
        assert_eq!(bf.name, "ws-json");
        assert_eq!(bf.coerce_workspace, Some(true));
    }

    #[test]
    fn test_synthesized_account_env_for_claude_aliases() {
        let env = synthesized_account_env_for_home(
            Provider::Claude,
            "yolo2",
            Path::new("/tmp/fake-home"),
        )
        .unwrap();
        assert_eq!(
            env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/tmp/fake-home/.claude-account2")
        );
    }

    #[test]
    fn test_synthesized_account_env_for_codex_account_name() {
        let env = synthesized_account_env_for_home(
            Provider::Codex,
            "account3",
            Path::new("/tmp/fake-home"),
        )
        .unwrap();
        assert_eq!(
            env.get("CODEX_HOME").map(String::as_str),
            Some("/tmp/fake-home/.codex-account3")
        );
    }

    #[test]
    fn test_resolve_provider_env_merges_config_overrides() {
        let store = temp_store();
        let mut config = load_config(store.path());
        config.accounts.insert(
            "account2".into(),
            Account {
                env: Some(HashMap::from([("EXTRA_FLAG".into(), "1".into())])),
                ..Default::default()
            },
        );
        save_config(&config, store.path());

        let resolved =
            resolve_provider_env(Provider::Claude, Some("account2"), None, store.path(), None)
                .unwrap();
        assert_eq!(resolved.get("EXTRA_FLAG").map(String::as_str), Some("1"));
        assert!(
            resolved
                .get("CLAUDE_CONFIG_DIR")
                .is_some_and(|path| path.ends_with("/.claude-account2"))
        );
    }

    #[test]
    fn test_effective_account_falls_back_to_provider_default() {
        let store = temp_store();
        let mut config = load_config(store.path());
        config.provider_defaults.insert(
            Provider::Claude,
            ProviderDefault {
                account: "account2".into(),
            },
        );
        save_config(&config, store.path());

        let effective = effective_account(Provider::Claude, None, store.path());
        assert_eq!(effective.as_deref(), Some("account2"));
    }

    #[test]
    fn test_resolve_provider_env_uses_provider_default_account() {
        let store = temp_store();
        let mut config = load_config(store.path());
        config.accounts.insert(
            "account2".into(),
            Account {
                env: Some(HashMap::from([("EXTRA_FLAG".into(), "1".into())])),
                ..Default::default()
            },
        );
        config.provider_defaults.insert(
            Provider::Claude,
            ProviderDefault {
                account: "account2".into(),
            },
        );
        save_config(&config, store.path());

        let resolved =
            resolve_provider_env(Provider::Claude, None, None, store.path(), None).unwrap();
        assert_eq!(resolved.get("EXTRA_FLAG").map(String::as_str), Some("1"));
        assert!(
            resolved
                .get("CLAUDE_CONFIG_DIR")
                .is_some_and(|path| path.ends_with("/.claude-account2"))
        );
    }

    #[test]
    fn test_resolve_provider_env_defaults_glm_claude_config() {
        let store = temp_store();
        let home = temp_store();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Glm, None, None, store.path(), None).unwrap()
        });
        assert!(!resolved.contains_key("OPENCODE_CONFIG"));
        // GLM now rides bro-harness on the Anthropic transport, not the
        // claude CLI; it selects the transport rather than CLAUDE_CONFIG_DIR.
        assert_eq!(
            resolved.get("BRO_HARNESS_TRANSPORT").map(String::as_str),
            Some("anthropic")
        );
        assert!(!resolved.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn test_resolve_provider_env_glm_account_override_updates_claude_config() {
        let store = temp_store();
        let home = temp_store();
        let mut config = load_config(store.path());
        config.accounts.insert(
            "zai2".into(),
            Account {
                env: Some(HashMap::from([(
                    "CLAUDE_CONFIG_DIR".into(),
                    home.path()
                        .join(".claude-zai-account2")
                        .to_string_lossy()
                        .into_owned(),
                )])),
                ..Default::default()
            },
        );
        save_config(&config, store.path());

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(
                Provider::Glm,
                Some("zai2"),
                Some("glm-4.7"),
                store.path(),
                None,
            )
            .unwrap()
        });
        assert!(
            resolved
                .get("CLAUDE_CONFIG_DIR")
                .is_some_and(|path| path.ends_with("/.claude-zai-account2"))
        );
    }

    #[test]
    fn test_build_opencode_config_includes_blackbox_md_in_instructions() {
        let home = temp_store();
        let blackbox_dir = home.path().join(".blackbox");
        fs::create_dir_all(&blackbox_dir).unwrap();
        let blackbox_md = blackbox_dir.join("BLACKBOX.md");
        fs::write(&blackbox_md, "# global guidance").unwrap();

        let config = with_fake_home(home.path(), || {
            build_opencode_config(Provider::Inception, None, false)
        });
        let instructions = config
            .get("instructions")
            .and_then(Value::as_array)
            .expect("instructions should be present when BLACKBOX.md exists");
        assert_eq!(instructions.len(), 1);
        assert_eq!(
            instructions[0].as_str(),
            Some(blackbox_md.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_build_opencode_config_omits_instructions_when_blackbox_md_missing() {
        let home = temp_store();
        let config = with_fake_home(home.path(), || {
            build_opencode_config(Provider::Inception, None, false)
        });
        assert!(
            config.get("instructions").is_none(),
            "instructions field should be absent when BLACKBOX.md does not exist"
        );
    }

    #[test]
    fn test_build_opencode_config_suppresses_instructions_when_requested() {
        let home = temp_store();
        let blackbox_dir = home.path().join(".blackbox");
        fs::create_dir_all(&blackbox_dir).unwrap();
        fs::write(blackbox_dir.join("BLACKBOX.md"), "# global guidance").unwrap();

        let config = with_fake_home(home.path(), || {
            build_opencode_config(Provider::Inception, None, true)
        });
        assert!(
            config.get("instructions").is_none(),
            "suppress_instructions=true must omit the instructions array even when BLACKBOX.md exists"
        );
    }

    #[test]
    fn test_resolve_provider_env_inception_uses_distinct_config_paths() {
        let store = temp_store();
        let home = temp_store();
        let blackbox_dir = home.path().join(".blackbox");
        fs::create_dir_all(&blackbox_dir).unwrap();
        fs::write(blackbox_dir.join("BLACKBOX.md"), "# global guidance").unwrap();

        let (first, second) = with_fake_home(home.path(), || {
            let first = resolve_provider_env(
                Provider::Inception,
                None,
                Some("inception/mercury-2"),
                store.path(),
                None,
            )
            .unwrap();
            let ctx = BrofileContext {
                provider_defaults: Some(ProviderDefaultsMode::StrictSuppress),
            };
            let second = resolve_provider_env(
                Provider::Inception,
                None,
                Some("inception/mercury-2"),
                store.path(),
                Some(&ctx),
            )
            .unwrap();
            (first, second)
        });

        let first_path = first.get("OPENCODE_CONFIG").unwrap();
        let second_path = second.get("OPENCODE_CONFIG").unwrap();
        assert_ne!(
            first_path, second_path,
            "different launches must not race through a provider-wide generated opencode config"
        );
        assert!(
            serde_json::from_str::<Value>(&fs::read_to_string(first_path).unwrap())
                .unwrap()
                .get("instructions")
                .is_some()
        );
        assert!(
            serde_json::from_str::<Value>(&fs::read_to_string(second_path).unwrap())
                .unwrap()
                .get("instructions")
                .is_none(),
            "strict suppression launch config must omit opencode instructions"
        );
    }

    #[test]
    fn test_enforce_provider_defaults_strict_unsupported_fails() {
        let ctx = BrofileContext {
            provider_defaults: Some(ProviderDefaultsMode::StrictSuppress),
        };
        let err = enforce_provider_defaults(Provider::Copilot, Some(&ctx)).expect_err(
            "strict_suppress on Copilot should fail closed until verified controls exist",
        );
        assert!(err.contains("error.provider_defaults_unsupported"));
        assert!(err.contains("copilot"));
    }

    #[test]
    fn test_enforce_provider_defaults_strict_supported_ok() {
        let ctx = BrofileContext {
            provider_defaults: Some(ProviderDefaultsMode::StrictSuppress),
        };
        enforce_provider_defaults(Provider::Inception, Some(&ctx))
            .expect("strict_suppress on Inception is honored");
        enforce_provider_defaults(Provider::Claude, Some(&ctx))
            .expect("strict_suppress on Claude is honored via --system-prompt override");
        enforce_provider_defaults(Provider::Codex, Some(&ctx))
            .expect("strict_suppress on Codex is honored via config overrides");
    }

    #[test]
    fn test_resolve_provider_env_codex_suppression_overlays_home_without_agents() {
        let store = temp_store();
        let home = temp_store();
        let codex_home = home.path().join(".codex");
        fs::create_dir_all(&codex_home).unwrap();
        fs::write(codex_home.join("AGENTS.md"), "global instructions").unwrap();
        fs::write(
            codex_home.join("AGENTS.override.md"),
            "override instructions",
        )
        .unwrap();
        fs::write(codex_home.join("config.toml"), "model = \"gpt-5.4\"\n").unwrap();
        fs::write(codex_home.join("auth.json"), "{}\n").unwrap();

        let ctx = BrofileContext {
            provider_defaults: Some(ProviderDefaultsMode::StrictSuppress),
        };
        let env = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Codex, None, None, store.path(), Some(&ctx)).unwrap()
        });
        let overlay = PathBuf::from(env.get("CODEX_HOME").unwrap());

        assert_ne!(overlay, codex_home);
        assert!(overlay.join("config.toml").exists());
        assert!(overlay.join("auth.json").exists());
        assert!(!overlay.join("AGENTS.md").exists());
        assert!(!overlay.join("AGENTS.override.md").exists());
    }

    #[test]
    fn test_enforce_provider_defaults_suppress_when_supported_never_fails() {
        let ctx = BrofileContext {
            provider_defaults: Some(ProviderDefaultsMode::SuppressWhenSupported),
        };
        enforce_provider_defaults(Provider::Claude, Some(&ctx))
            .expect("suppress_when_supported is best-effort and never fails closed");
    }

    #[test]
    fn test_resolve_provider_env_defaults_deepseek_claude_config() {
        let store = temp_store();
        let home = temp_store();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Deepseek, None, None, store.path(), None).unwrap()
        });
        assert!(!resolved.contains_key("OPENCODE_CONFIG"));
        // DeepSeek now rides bro-harness on the Anthropic transport.
        assert_eq!(
            resolved.get("BRO_HARNESS_TRANSPORT").map(String::as_str),
            Some("anthropic")
        );
        assert!(!resolved.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn test_resolve_provider_env_brodex_selects_responses_transport() {
        let store = temp_store();
        let home = temp_store();
        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Brodex, None, None, store.path(), None).unwrap()
        });
        assert_eq!(
            resolved.get("BRO_HARNESS_TRANSPORT").map(String::as_str),
            Some("openai-responses")
        );
        // No OPENAI_API_KEY → harness uses Codex ChatGPT OAuth from CODEX_HOME.
        assert!(!resolved.contains_key("OPENAI_API_KEY"));
    }

    #[test]
    fn test_resolve_provider_env_vibebh_selects_chat_transport_and_mistral() {
        let store = temp_store();
        let home = temp_store();
        let resolved = with_fake_home(home.path(), || {
            // Isolate from any real MISTRAL_API_KEY in the process env so the
            // ~/.vibe/.env fallback path is exercised deterministically.
            let prior_key = std::env::var_os("MISTRAL_API_KEY");
            unsafe {
                std::env::remove_var("MISTRAL_API_KEY");
            }
            let vibe_dir = home.path().join(".vibe");
            fs::create_dir_all(&vibe_dir).unwrap();
            fs::write(vibe_dir.join(".env"), "MISTRAL_API_KEY=\"test-mistral-key\"\n").unwrap();

            let resolved =
                resolve_provider_env(Provider::VibeBh, None, None, store.path(), None).unwrap();

            if let Some(value) = prior_key {
                unsafe { std::env::set_var("MISTRAL_API_KEY", value) }
            }
            resolved
        });
        assert_eq!(
            resolved.get("BRO_HARNESS_TRANSPORT").map(String::as_str),
            Some("openai-chat")
        );
        assert_eq!(
            resolved.get("BRO_HARNESS_CHAT_REASONING").map(String::as_str),
            Some("mistral")
        );
        assert_eq!(
            resolved.get("OPENAI_BASE_URL").map(String::as_str),
            Some("https://api.mistral.ai/v1")
        );
        // Key lifted from ~/.vibe/.env (quotes stripped).
        assert_eq!(
            resolved.get("OPENAI_API_KEY").map(String::as_str),
            Some("test-mistral-key")
        );
    }

    #[test]
    fn test_resolve_provider_env_defaults_inception_opencode_config() {
        let store = temp_store();

        let resolved =
            resolve_provider_env(Provider::Inception, None, None, store.path(), None).unwrap();
        let config_path = resolved.get("OPENCODE_CONFIG").unwrap();
        assert!(config_path.contains("inception-opencode-"));
        assert!(config_path.ends_with(".json"));
        let config = fs::read_to_string(config_path).unwrap();
        assert!(config.contains("\"model\": \"inception/mercury-2\""));
        assert!(config.contains("\"small_model\": \"inception/mercury-2\""));
        assert!(!config.contains("mercury-edit-2"));
    }
}
