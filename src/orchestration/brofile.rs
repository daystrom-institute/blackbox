use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rmcp::schemars;
use serde::{Deserialize, Serialize};

use super::allocator::RuntimeRequest;
use super::mcp::McpFilters;
use super::providers::{Capability, Provider};

// ---------------------------------------------------------------------------
// Brofile
// ---------------------------------------------------------------------------

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
    /// Optional tool-surface selector. When set, the daemon evaluates the
    /// installed surface packet for this surface (the same `evaluate_tool_surface`
    /// authority the rmcp wire head uses for `?surface=<id>` callers) and folds
    /// the verdict into the dispatch filter plane, so an in-process session is
    /// surface-governed exactly like a wire caller. Unset → no surface fold
    /// (recursion-guard + `filters` still apply). See
    /// design/bro-harness/harness-daemon-boundary.md §6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
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
    /// Optional code-mode selector (harness-backed providers). Sets the
    /// authorial tool surface for sessions this brofile dispatches:
    /// `off`/`optional`/`only`. Unset → harness default (`optional`). A
    /// per-dispatch `ExecParams.code_mode` overrides this. See
    /// `crate::orchestration::brofile::CodeMode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_mode: Option<CodeMode>,
    /// Optional provider service tier. For Brodex/OpenAI Responses, `priority`
    /// is Codex `/fast`; `default` clears back to the backend default. A
    /// per-dispatch service-tier override wins over this brofile value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
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

/// Code-mode selector carried to the harness as `--code-mode`. Mirrors the
/// harness-side `CodeMode` (Codex's `ToolMode`): `off` (flat tools only),
/// `optional` (flat tools + `exec`/`wait`), `only` (`exec`/`wait` authorial
/// surface, flat builtins hidden). When unset the harness applies its default
/// (`optional`). Only harness-backed providers honor it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum CodeMode {
    Off,
    #[default]
    Optional,
    Only,
}

impl CodeMode {
    /// The `--code-mode` token / serde value.
    pub fn as_str(self) -> &'static str {
        match self {
            CodeMode::Off => "off",
            CodeMode::Optional => "optional",
            CodeMode::Only => "only",
        }
    }
}

/// Whether `provider` can honor a non-`Default` `ProviderDefaultsMode`
/// in v1. Only surviving harness providers advertise this; removed CLI-backed
/// providers fail closed before environment or arg construction.
pub fn provider_supports_defaults_suppression(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh
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

/// Harness env for the Anthropic-transport providers (GLM, DeepSeek, MiniMax).
/// These no longer run the `claude` CLI; they run `bro-harness`, which reads
/// `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN` from its own env. We lift
/// those out of the operator's existing `~/.claude-{zai,ds,mm}/settings.json`
/// `env` block (the same credentials the CLI used) and select the transport.
fn default_claude_compatible_env(
    provider: Provider,
    home_dir: &Path,
) -> Option<HashMap<String, String>> {
    let rel_path = match provider {
        Provider::Glm => ".claude-zai",
        Provider::Deepseek => ".claude-ds",
        Provider::Minimax => ".claude-mm",
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
        Provider::Brodex => ("CODEX_HOME", format!(".codex{suffix}")),
        // GLM/DeepSeek/MiniMax inherit credentials from fixed
        // Claude-compatible config dirs; vibe-bh authenticates via
        // MISTRAL_API_KEY, not accounts.
        Provider::Glm
        | Provider::Deepseek
        | Provider::Minimax
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
    _model: Option<&str>,
    store_dir: &Path,
    brofile_context: Option<&BrofileContext>,
) -> Option<HashMap<String, String>> {
    if !provider.is_dispatchable() {
        return None;
    }
    let account_name = effective_account(provider, account_name, store_dir);
    let suppress_instructions = brofile_context
        .and_then(|c| c.provider_defaults)
        .map(ProviderDefaultsMode::suppresses)
        .unwrap_or(false);
    let mut env = match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => dirs::home_dir()
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
        Provider::Workflow => HashMap::new(),
    };

    if let Some(account_name) = account_name.as_deref() {
        if !matches!(
            provider,
            Provider::Glm | Provider::Deepseek | Provider::Minimax | Provider::VibeBh
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

    if provider == Provider::Brodex && suppress_instructions {
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
            provider: Provider::Glm,
            account: None,
            lens: Some("You are a code reviewer".into()),
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("reviewer", dir.path(), None);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.name, "reviewer");
        assert_eq!(loaded.provider, Provider::Glm);
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
            provider: Provider::Brodex,
            account: None,
            lens: Some("test".into()),
            model: Some("gpt-5.5".into()),
            effort: Some("medium".into()),
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: Some("priority".into()),
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
            provider: Provider::Glm,
            account: None,
            lens: Some("global lens".into()),
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
        };
        save_brofile(&global_bf, "global", store.path(), None).expect("brofile save");

        let project_bf = Brofile {
            name: "worker".into(),
            provider: Provider::Brodex,
            account: None,
            lens: Some("project lens".into()),
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
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
        assert_eq!(resolved.unwrap().provider, Provider::Brodex);
    }

    #[test]
    fn test_list_brofiles() {
        let dir = temp_store();
        for name in &["alpha", "beta", "gamma"] {
            let bf = Brofile {
                name: name.to_string(),
                provider: Provider::Glm,
                account: None,
                lens: None,
                model: None,
                effort: None,
                filters: None,
                surface: None,
                coerce_workspace: None,
                runtime: None,
                context: None,
                code_mode: None,
                service_tier: None,
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
            provider: Provider::Deepseek,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
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
            provider: Provider::Glm,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: Some(McpFilters {
                allow: vec![],
                disallow: vec!["mcp__blackbox__bro_*".into(), "Bash(*)".into()],
            }),
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("auditor", dir.path(), None).unwrap();
        let f = loaded.filters.expect("filters round-trip");
        assert_eq!(f.disallow.len(), 2);
        assert!(f.disallow.contains(&"Bash(*)".to_string()));
        assert!(f.allow.is_empty());
    }

    #[test]
    fn test_brofile_persists_surface() {
        let dir = temp_store();
        let bf = Brofile {
            name: "readonly-persona".into(),
            provider: Provider::Glm,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            surface: Some("readonly".into()),
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("readonly-persona", dir.path(), None).unwrap();
        assert_eq!(loaded.surface.as_deref(), Some("readonly"));
    }

    #[test]
    fn rust_refactor_persona_matches_design_spec() {
        let src =
            include_str!("../../system-defaults/brofiles/refactor/rust-refactor-persona.json");
        let bf: Brofile = serde_json::from_str(src).expect("rust-refactor-persona parses");
        assert_eq!(bf.name, "rust-refactor-persona");
        assert_eq!(bf.provider, Provider::Glm);
        assert_eq!(
            bf.context.as_ref().and_then(|c| c.provider_defaults),
            Some(ProviderDefaultsMode::SuppressWhenSupported)
        );
        assert!(
            bf.lens
                .as_deref()
                .unwrap_or("")
                .contains("in-box refactor bindings")
        );

        let f = bf.filters.expect("filters present");

        let expected_allow: Vec<&str> = vec![
            "mcp__blackbox__bbox_note",
            "mcp__blackbox__bbox_thread",
            "mcp__blackbox__bbox_pin",
            "mcp__blackbox__bbox_inspect_entity",
            "mcp__blackbox__bbox_hybrid_search",
            "Read",
            "Grep",
            "Glob",
            "exec",
            "wait",
            "code.*",
            "edits.*",
            "lsp.*",
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
        assert_eq!(bf.provider, Provider::Brodex);
        assert_eq!(bf.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(bf.effort.as_deref(), Some("medium"));
        assert_eq!(
            bf.context.as_ref().and_then(|c| c.provider_defaults),
            Some(ProviderDefaultsMode::SuppressWhenSupported)
        );
        let lens = bf.lens.as_deref().unwrap_or("");
        assert!(lens.contains("in-box refactor bindings"));
        assert!(
            lens.contains("Lombok") || lens.contains("annotation"),
            "Java lens should reference Java-specific caveats (Lombok / annotation processors)"
        );

        let f = bf.filters.expect("filters present");

        let expected_allow: Vec<&str> = vec![
            "mcp__blackbox__bbox_note",
            "mcp__blackbox__bbox_thread",
            "mcp__blackbox__bbox_pin",
            "mcp__blackbox__bbox_inspect_entity",
            "mcp__blackbox__bbox_hybrid_search",
            "Read",
            "Grep",
            "Glob",
            "exec",
            "wait",
            "code.*",
            "edits.*",
            "java.*",
            "analysis.*",
            "lsp.*",
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
    fn phase_decomposer_edit_implementer_parses() {
        let src =
            include_str!("../../system-defaults/brofiles/phase-decompose/edit-implementer.json");
        let bf: Brofile = serde_json::from_str(src).expect("phase-decomposer edit brofile parses");
        assert_eq!(bf.name, "phase-decomposer-edit-implementer");
        assert_eq!(bf.provider, Provider::Brodex);
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

    /// Rust + Java refactor personas share a core allow/disallow surface
    /// (exec/wait + code.* facts + edits.* algebra + the grounding MCP tools)
    /// and differ only in language-scoped binding extensions (java: java.* /
    /// analysis.* / lsp.*; rust: lsp.*). The core symmetry is load-bearing
    /// for refactor-atom interchange between brofiles; the extensions mirror
    /// what the bindings actually implement per language.
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
        // The cell bindings are language-scoped: java adds the java.* /
        // analysis.* / lsp.* namespaces, rust adds lsp.* only; the shared core
        // (exec/wait + code.* facts + edits.* algebra) must match. build.* is
        // deliberately absent everywhere: build.gate accepts arbitrary shell
        // commands, which would bypass the Bash/Write/Edit denial.
        const JAVA_ONLY: &[&str] = &["java.*", "analysis.*", "lsp.*"];
        const RUST_ONLY: &[&str] = &["lsp.*"];
        let r_core: std::collections::BTreeSet<&str> = r_allow
            .iter()
            .copied()
            .filter(|t| !RUST_ONLY.contains(t))
            .collect();
        let j_core: std::collections::BTreeSet<&str> = j_allow
            .iter()
            .copied()
            .filter(|t| !JAVA_ONLY.contains(t))
            .collect();
        assert_eq!(
            r_core, j_core,
            "refactor personas should expose identical core allow sets"
        );
        for t in JAVA_ONLY {
            assert!(j_allow.contains(t), "java persona missing {t}");
        }
        assert!(
            !r_allow.contains("java.*") && !r_allow.contains("analysis.*"),
            "rust persona must not advertise java-scoped bindings"
        );
        assert!(
            !r_allow.contains("build.*") && !j_allow.contains("build.*"),
            "build.gate is an arbitrary-shell bypass and stays off personas"
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

    /// Regression scan (post-MCP-retirement): live brofile lenses, allowlists,
    /// and agent prompt contracts must never name the retired daemon MCP
    /// refactor/slice/code-nav/macro tools. Retired names in an allowlist are
    /// dead entries; in a lens or prompt they instruct agents to call tools
    /// that no longer exist. Atom/workflow/eval artifacts are deliberately
    /// out of scope here (their content migration is a separate arc).
    #[test]
    fn live_lenses_and_prompts_never_name_retired_mcp_tools() {
        const RETIRED: &[&str] = &[
            "bbox_refactor_",
            "bbox_slice_",
            "bbox_code_query",
            "bbox_code_symbols",
            "bbox_code_node_describe",
            "bbox_code_refs",
            "bbox_code_usages",
            "bbox_code_implementations",
            "bbox_code_type_at",
            "bbox_code_outline",
            "bbox_workspace_symbols",
            "bbox_code_*",
            // Exact macro tool names; a bare "macro_" substring would false-
            // positive on legitimate lens text like `acknowledge_defmacro_move`.
            "macro_list",
            "macro_describe",
            "macro_validate",
            "macro_register",
            "macro_unregister",
            "macro_plan",
            "macro_apply",
            "macro_run",
        ];
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        for dir in [
            "system-defaults/brofiles",
            ".bbox/brofiles",
            "prompts/agents",
        ] {
            for entry in walkdir::WalkDir::new(crate_root.join(dir))
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_type().is_file()
                        && matches!(
                            e.path().extension().and_then(|x| x.to_str()),
                            Some("json") | Some("md")
                        )
                })
            {
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                for name in RETIRED {
                    if text.contains(name) {
                        offenders.push(format!("{}: {name}", entry.path().display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "retired MCP tool names in live lenses/prompts:\n{}",
            offenders.join("\n")
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
        assert_eq!(bf.provider, Provider::Brodex);
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
            provider: Provider::Brodex,
            account: None,
            lens: None,
            model: Some("gpt-5.4-mini".into()),
            effort: Some("low".into()),
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("fast", dir.path(), None).unwrap();
        assert_eq!(loaded.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(loaded.effort.as_deref(), Some("low"));
    }

    #[test]
    fn test_brofile_code_mode_round_trip() {
        let dir = temp_store();
        let bf = Brofile {
            name: "coder".into(),
            provider: Provider::Glm,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: Some(CodeMode::Only),
            service_tier: Some("priority".into()),
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("coder", dir.path(), None).unwrap();
        assert_eq!(loaded.code_mode, Some(CodeMode::Only));
        assert_eq!(loaded.service_tier.as_deref(), Some("priority"));

        // Absent in JSON ⇒ None (serde default), and the token is snake_case.
        let json = r#"{"name":"plain","provider":"glm"}"#;
        let plain: Brofile = serde_json::from_str(json).unwrap();
        assert_eq!(plain.code_mode, None);
        assert_eq!(plain.service_tier, None);
        assert_eq!(CodeMode::Only.as_str(), "only");
        let parsed: Brofile = serde_json::from_str(
            r#"{"name":"x","provider":"glm","code_mode":"optional","service_tier":"priority"}"#,
        )
        .unwrap();
        assert_eq!(parsed.code_mode, Some(CodeMode::Optional));
        assert_eq!(parsed.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn test_brofile_coerce_workspace_round_trip() {
        let dir = temp_store();
        let bf = Brofile {
            name: "ws-worker".into(),
            provider: Provider::Glm,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: Some(true),
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
        };
        save_brofile(&bf, "global", dir.path(), None).expect("brofile save");
        let loaded = resolve_brofile("ws-worker", dir.path(), None).unwrap();
        assert_eq!(loaded.coerce_workspace, Some(true));

        let bf_off = Brofile {
            name: "ws-off".into(),
            provider: Provider::Glm,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
            surface: None,
            coerce_workspace: None,
            runtime: None,
            context: None,
            code_mode: None,
            service_tier: None,
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
    fn test_synthesized_account_env_skips_fixed_claude_compatible_dirs() {
        assert!(
            synthesized_account_env_for_home(Provider::Glm, "yolo2", Path::new("/tmp/fake-home"))
                .is_none()
        );
        assert!(
            synthesized_account_env_for_home(
                Provider::Minimax,
                "yolo2",
                Path::new("/tmp/fake-home"),
            )
            .is_none()
        );
    }

    #[test]
    fn test_synthesized_account_env_for_codex_account_name() {
        let env = synthesized_account_env_for_home(
            Provider::Brodex,
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
            resolve_provider_env(Provider::Glm, Some("account2"), None, store.path(), None)
                .unwrap();
        assert_eq!(resolved.get("EXTRA_FLAG").map(String::as_str), Some("1"));
        assert!(!resolved.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn test_effective_account_falls_back_to_provider_default() {
        let store = temp_store();
        let mut config = load_config(store.path());
        config.provider_defaults.insert(
            Provider::Glm,
            ProviderDefault {
                account: "account2".into(),
            },
        );
        save_config(&config, store.path());

        let effective = effective_account(Provider::Glm, None, store.path());
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
            Provider::Glm,
            ProviderDefault {
                account: "account2".into(),
            },
        );
        save_config(&config, store.path());

        let resolved = resolve_provider_env(Provider::Glm, None, None, store.path(), None).unwrap();
        assert_eq!(resolved.get("EXTRA_FLAG").map(String::as_str), Some("1"));
        assert!(!resolved.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn test_resolve_provider_env_defaults_glm_claude_config() {
        let store = temp_store();
        let home = temp_store();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Glm, None, None, store.path(), None).unwrap()
        });
        // GLM now rides bro-harness on the Anthropic transport, not the
        // claude CLI; it selects the transport rather than CLAUDE_CONFIG_DIR.
        assert_eq!(
            resolved.get("BRO_HARNESS_TRANSPORT").map(String::as_str),
            Some("anthropic")
        );
        assert!(!resolved.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn test_resolve_provider_env_account_env_carries_web_search_flag() {
        let store = temp_store();
        let home = temp_store();
        let mut config = load_config(store.path());
        config.accounts.insert(
            "zai-nosearch".into(),
            Account {
                env: Some(HashMap::from([(
                    "BRO_HARNESS_WEB_SEARCH".into(),
                    "0".into(),
                )])),
                ..Default::default()
            },
        );
        save_config(&config, store.path());

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(
                Provider::Glm,
                Some("zai-nosearch"),
                None,
                store.path(),
                None,
            )
            .unwrap()
        });
        // No lane-level default exists for BRO_HARNESS_WEB_SEARCH; the knob is
        // operator-scoped (account env here, or per-dispatch env_overrides),
        // and the harness resolves it via session env (web_search_enabled).
        assert_eq!(
            resolved.get("BRO_HARNESS_WEB_SEARCH").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn test_resolve_provider_env_minimax_claude_config() {
        let store = temp_store();
        let home = temp_store();
        let settings_dir = home.path().join(".claude-mm");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(
            settings_dir.join("settings.json"),
            r#"{
              "env": {
                "ANTHROPIC_BASE_URL": "https://api.minimax.io/anthropic",
                "ANTHROPIC_AUTH_TOKEN": "test-token",
                "ANTHROPIC_MODEL": "MiniMax-M3"
              },
              "model": "MiniMax-M3"
            }"#,
        )
        .unwrap();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Minimax, None, None, store.path(), None).unwrap()
        });
        assert_eq!(
            resolved.get("BRO_HARNESS_TRANSPORT").map(String::as_str),
            Some("anthropic")
        );
        assert_eq!(
            resolved.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://api.minimax.io/anthropic")
        );
        assert_eq!(
            resolved.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("test-token")
        );
        assert!(!resolved.contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn test_resolve_provider_env_glm_account_override_can_set_claude_config() {
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
    fn test_enforce_provider_defaults_strict_unsupported_fails() {
        let ctx = BrofileContext {
            provider_defaults: Some(ProviderDefaultsMode::StrictSuppress),
        };
        let err = enforce_provider_defaults(Provider::Workflow, Some(&ctx))
            .expect_err("strict_suppress on workflow should fail closed");
        assert!(err.contains("error.provider_defaults_unsupported"));
        assert!(err.contains("workflow"));
    }

    #[test]
    fn test_enforce_provider_defaults_strict_supported_ok() {
        let ctx = BrofileContext {
            provider_defaults: Some(ProviderDefaultsMode::StrictSuppress),
        };
        enforce_provider_defaults(Provider::Glm, Some(&ctx))
            .expect("strict_suppress on Inception is honored");
        enforce_provider_defaults(Provider::Glm, Some(&ctx))
            .expect("strict_suppress on Claude is honored via --system-prompt override");
        enforce_provider_defaults(Provider::Brodex, Some(&ctx))
            .expect("strict_suppress on Codex is honored via config overrides");
        enforce_provider_defaults(Provider::Minimax, Some(&ctx))
            .expect("strict_suppress on MiniMax is honored via --system-prompt override");
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
            resolve_provider_env(Provider::Brodex, None, None, store.path(), Some(&ctx)).unwrap()
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
        enforce_provider_defaults(Provider::Glm, Some(&ctx))
            .expect("suppress_when_supported is best-effort and never fails closed");
    }

    #[test]
    fn test_resolve_provider_env_defaults_deepseek_claude_config() {
        let store = temp_store();
        let home = temp_store();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Deepseek, None, None, store.path(), None).unwrap()
        });
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
            fs::write(
                vibe_dir.join(".env"),
                "MISTRAL_API_KEY=\"test-mistral-key\"\n",
            )
            .unwrap();

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
            resolved
                .get("BRO_HARNESS_CHAT_REASONING")
                .map(String::as_str),
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
}
