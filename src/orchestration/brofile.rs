use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BroConfig {
    #[serde(default)]
    pub accounts: HashMap<String, Account>,
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
}
