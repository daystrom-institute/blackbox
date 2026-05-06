use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::mcp::McpFilters;
use super::providers::Provider;

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
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
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

pub fn save_brofile(bf: &Brofile, scope: &str, store_dir: &Path, project_dir: Option<&str>) {
    let dir = if scope == "project" {
        project_brofiles_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        brofiles_dir(store_dir)
    };
    let _ = fs::create_dir_all(&dir);
    let file = dir.join(format!("{}.json", bf.name));
    let tmp = dir.join(format!("{}.json.tmp", bf.name));
    if let Ok(data) = serde_json::to_string_pretty(bf) {
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(data.as_bytes());
            let _ = f.sync_all();
            let _ = fs::rename(&tmp, &file);
        }
    }
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
    store_dir
        .join("generated")
        .join(format!("{}-opencode.json", provider.as_str()))
}

struct OpencodeProfile {
    default_model: &'static str,
    small_model: &'static str,
}

fn opencode_profile(provider: Provider) -> Option<OpencodeProfile> {
    match provider {
        Provider::Glm => Some(OpencodeProfile {
            default_model: "zai-coding-plan/glm-5.1",
            small_model: "zai-coding-plan/glm-4.5-air",
        }),
        Provider::Deepseek => Some(OpencodeProfile {
            default_model: "deepseek/deepseek-v4-pro",
            small_model: "deepseek/deepseek-chat",
        }),
        Provider::Inception => Some(OpencodeProfile {
            default_model: "inception/mercury-2",
            small_model: "inception/mercury-2",
        }),
        _ => None,
    }
}

fn build_opencode_config(provider: Provider, model: Option<&str>) -> Value {
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
    if let Some(home) = dirs::home_dir() {
        let blackbox_md = crate::util::blackbox_global_common_md_path(&home);
        if blackbox_md.exists() {
            config["instructions"] =
                serde_json::json!([blackbox_md.to_string_lossy().into_owned()]);
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
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let config_path = default_opencode_config_path(store_dir, provider);
    write_json_file(&config_path, &build_opencode_config(provider, model));
    env.insert(
        "OPENCODE_CONFIG".into(),
        config_path.to_string_lossy().into_owned(),
    );
    env
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
        Provider::Codex => ("CODEX_HOME", format!(".codex{suffix}")),
        // `gh` respects GH_CONFIG_DIR; keep the same account suffix pattern.
        Provider::Copilot => ("GH_CONFIG_DIR", format!(".config/gh{suffix}")),
        Provider::Glm
        | Provider::Deepseek
        | Provider::Inception
        | Provider::Gemini
        | Provider::Vibe => return None,
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
) -> Option<HashMap<String, String>> {
    let account_name = effective_account(provider, account_name, store_dir);
    let mut env = match provider {
        Provider::Glm | Provider::Deepseek | Provider::Inception => {
            default_opencode_env(provider, store_dir, model)
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
        let prior = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        let result = f();
        match prior {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
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
        };
        save_brofile(&bf, "global", dir.path(), None);
        let loaded = resolve_brofile("reviewer", dir.path(), None);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.name, "reviewer");
        assert_eq!(loaded.provider, Provider::Claude);
        assert_eq!(loaded.lens.as_deref(), Some("You are a code reviewer"));
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
        };
        save_brofile(&global_bf, "global", store.path(), None);

        let project_bf = Brofile {
            name: "worker".into(),
            provider: Provider::Codex,
            account: None,
            lens: Some("project lens".into()),
            model: None,
            effort: None,
            filters: None,
        };
        save_brofile(
            &project_bf,
            "project",
            store.path(),
            Some(project.path().to_str().unwrap()),
        );

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
            };
            save_brofile(&bf, "global", dir.path(), None);
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
        };
        save_brofile(&bf, "global", dir.path(), None);
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
        };
        save_brofile(&bf, "global", dir.path(), None);
        let loaded = resolve_brofile("auditor", dir.path(), None).unwrap();
        let f = loaded.filters.expect("filters round-trip");
        assert_eq!(f.disallow.len(), 2);
        assert!(f.disallow.contains(&"Bash(*)".to_string()));
        assert!(f.allow.is_empty());
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
        };
        save_brofile(&bf, "global", dir.path(), None);
        let loaded = resolve_brofile("fast", dir.path(), None).unwrap();
        assert_eq!(loaded.model.as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(loaded.effort.as_deref(), Some("low"));
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
            },
        );
        save_config(&config, store.path());

        let resolved =
            resolve_provider_env(Provider::Claude, Some("account2"), None, store.path()).unwrap();
        assert_eq!(resolved.get("EXTRA_FLAG").map(String::as_str), Some("1"));
        assert!(resolved
            .get("CLAUDE_CONFIG_DIR")
            .is_some_and(|path| path.ends_with("/.claude-account2")));
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
            },
        );
        config.provider_defaults.insert(
            Provider::Claude,
            ProviderDefault {
                account: "account2".into(),
            },
        );
        save_config(&config, store.path());

        let resolved = resolve_provider_env(Provider::Claude, None, None, store.path()).unwrap();
        assert_eq!(resolved.get("EXTRA_FLAG").map(String::as_str), Some("1"));
        assert!(resolved
            .get("CLAUDE_CONFIG_DIR")
            .is_some_and(|path| path.ends_with("/.claude-account2")));
    }

    #[test]
    fn test_resolve_provider_env_defaults_glm_opencode_config() {
        let store = temp_store();
        let home = temp_store();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(Provider::Glm, None, None, store.path()).unwrap()
        });
        assert!(!resolved.contains_key("ANTHROPIC_AUTH_TOKEN"));
        let config_path = resolved.get("OPENCODE_CONFIG").unwrap();
        assert!(config_path.ends_with("glm-opencode.json"));
        let config = fs::read_to_string(config_path).unwrap();
        assert!(config.contains("\"model\": \"zai-coding-plan/glm-5.1\""));
        assert!(config.contains("\"small_model\": \"zai-coding-plan/glm-4.5-air\""));
        assert!(config.contains("\"blackbox_bro_*\": false"));
    }

    #[test]
    fn test_resolve_provider_env_glm_model_override_updates_opencode_config() {
        let store = temp_store();
        let home = temp_store();

        let resolved = with_fake_home(home.path(), || {
            resolve_provider_env(
                Provider::Glm,
                Some("yoloz"),
                Some("zai-coding-plan/glm-4.7"),
                store.path(),
            )
            .unwrap()
        });
        let config_path = resolved.get("OPENCODE_CONFIG").unwrap();
        let config = fs::read_to_string(config_path).unwrap();
        assert!(config.contains("\"model\": \"zai-coding-plan/glm-4.7\""));
    }

    #[test]
    fn test_build_opencode_config_defaults_deepseek_model() {
        let config = build_opencode_config(Provider::Deepseek, None);
        assert_eq!(
            config.get("model").and_then(Value::as_str),
            Some("deepseek/deepseek-v4-pro")
        );
        assert_eq!(
            config.get("small_model").and_then(Value::as_str),
            Some("deepseek/deepseek-chat")
        );
        assert!(config.get("provider").is_none());
    }

    #[test]
    fn test_build_opencode_config_includes_blackbox_md_in_instructions() {
        let home = temp_store();
        let blackbox_dir = home.path().join(".blackbox");
        fs::create_dir_all(&blackbox_dir).unwrap();
        let blackbox_md = blackbox_dir.join("BLACKBOX.md");
        fs::write(&blackbox_md, "# global guidance").unwrap();

        let config = with_fake_home(home.path(), || build_opencode_config(Provider::Glm, None));
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
        let config = with_fake_home(home.path(), || build_opencode_config(Provider::Glm, None));
        assert!(
            config.get("instructions").is_none(),
            "instructions field should be absent when BLACKBOX.md does not exist"
        );
    }

    #[test]
    fn test_resolve_provider_env_defaults_deepseek_opencode_config() {
        let store = temp_store();

        let resolved = resolve_provider_env(Provider::Deepseek, None, None, store.path()).unwrap();
        let config_path = resolved.get("OPENCODE_CONFIG").unwrap();
        assert!(config_path.ends_with("deepseek-opencode.json"));
        let config = fs::read_to_string(config_path).unwrap();
        assert!(config.contains("\"model\": \"deepseek/deepseek-v4-pro\""));
        assert!(config.contains("\"small_model\": \"deepseek/deepseek-chat\""));
    }

    #[test]
    fn test_resolve_provider_env_defaults_inception_opencode_config() {
        let store = temp_store();

        let resolved = resolve_provider_env(Provider::Inception, None, None, store.path()).unwrap();
        let config_path = resolved.get("OPENCODE_CONFIG").unwrap();
        assert!(config_path.ends_with("inception-opencode.json"));
        let config = fs::read_to_string(config_path).unwrap();
        assert!(config.contains("\"model\": \"inception/mercury-2\""));
        assert!(config.contains("\"small_model\": \"inception/mercury-2\""));
        assert!(!config.contains("mercury-edit-2"));
    }
}
