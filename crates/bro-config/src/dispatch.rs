//! Dispatch reads `fleet.json` beside the selected `config.toml`.
//! Unknown fields are ignored. This module never rewrites configuration files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{McpServerConfig, client};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DispatchConfig {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub project_dispatch: BTreeMap<String, ProjectDispatch>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectDispatch {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl DispatchConfig {
    pub fn path() -> Option<PathBuf> {
        client::selected_config_path().and_then(|p| p.parent().map(|d| d.join("fleet.json")))
    }

    /// Missing, unreadable or malformed configuration yields empty defaults.
    pub fn load() -> Self {
        Self::path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(cfg) => cfg,
            Err(error) => {
                tracing::warn!(path = %path.display(), "ignoring fleet.json (parse failed): {error:#}");
                Self::default()
            }
        }
    }

    pub fn project_dispatch_for(&self, repo: &Path) -> Option<&ProjectDispatch> {
        lookup_project(&self.project_dispatch, repo)
    }
}

fn lookup_project<'a, T>(map: &'a BTreeMap<String, T>, repo: &Path) -> Option<&'a T> {
    if map.is_empty() {
        return None;
    }
    let canon = repo.canonicalize().ok();
    for cand in [canon.as_deref(), Some(repo)].into_iter().flatten() {
        if let Some(v) = map.get(cand.to_string_lossy().as_ref()) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_config_retains_active_settings_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("fleet.json");
        let original = json!({
            "mcpServers": {
                "http": {"type":"http", "url":"https://example.test/mcp", "headers":{"Authorization":{"$secret":"FIXTURE_TOKEN"}}},
                "sse": {"type":"sse", "url":"https://example.test/sse"},
                "stdio": {"type":"stdio", "command":"fixture", "args":["serve"], "env":{"KEY":{"$secret":"FIXTURE_KEY"}}}
            },
            "project_dispatch": {root.to_string_lossy().as_ref(): {"env":{"RUSTC_WRAPPER":"sccache"}, "seed_dirs":["target"]}},
            "classifier": {"enabled":true},
            "display": {"showReports":false},
            "projects": {"fixture":"/missing/fixture"},
            "pinTools": ["Read"],
            "code_mode": "only",
            "project_closeout": {"/missing/fixture":{"target":"main"}},
            "unknown": "x".repeat(100_000)
        });
        let bytes = serde_json::to_vec_pretty(&original).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let cfg = DispatchConfig::load_from(&path);
        assert_eq!(
            serde_json::to_value(&cfg.mcp_servers).unwrap(),
            original["mcpServers"]
        );
        assert_eq!(
            cfg.project_dispatch_for(&root).unwrap().env["RUSTC_WRAPPER"],
            "sccache"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn absent_and_invalid_config_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let cfg = DispatchConfig::load_from(&path);
        assert!(cfg.mcp_servers.is_empty() && cfg.project_dispatch.is_empty());
        for text in [
            "{}",
            "{broken",
            r#"{"project_dispatch":{"/fixture":{"env":{"KEY":42}}}}"#,
            r#"{"mcpServers":{"bad":{"type":"unknown"}}}"#,
        ] {
            std::fs::write(&path, text).unwrap();
            let cfg = DispatchConfig::load_from(&path);
            assert!(
                cfg.mcp_servers.is_empty() && cfg.project_dispatch.is_empty(),
                "{text}"
            );
        }
    }

    #[test]
    fn project_lookup_canonicalizes_and_keeps_missing_path_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("child")).unwrap();
        let missing = root.join("missing");
        let cfg: DispatchConfig = serde_json::from_value(json!({
            "project_dispatch": {
                root.to_string_lossy().as_ref(): {"env":{"MARKER":"canonical"}},
                missing.to_string_lossy().as_ref(): {"env":{"MARKER":"raw"}}
            }
        }))
        .unwrap();
        assert_eq!(
            cfg.project_dispatch_for(&root.join("child/.."))
                .unwrap()
                .env["MARKER"],
            "canonical"
        );
        assert_eq!(
            cfg.project_dispatch_for(&missing).unwrap().env["MARKER"],
            "raw"
        );
        assert!(
            cfg.project_dispatch_for(&root.join("unconfigured"))
                .is_none()
        );
    }

    #[test]
    fn selected_config_controls_dispatch_file_location() {
        let _guard = crate::test_env_lock();
        let _restore = crate::SavedEnv::capture("BLACKBOX_CONFIG");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("fleet.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"fixture":{"type":"stdio","command":"fixture"}}}"#,
        )
        .unwrap();
        // SAFETY: all environment-mutating tests hold the shared lock.
        unsafe {
            std::env::set_var("BLACKBOX_CONFIG", root.join("selected.toml"));
        }
        assert_eq!(DispatchConfig::path(), Some(path));
        assert_eq!(DispatchConfig::load().mcp_servers.len(), 1);
    }
}
